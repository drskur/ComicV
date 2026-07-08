use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use image::{DynamicImage, ExtendedColorType, ImageEncoder, RgbImage};
use tauri::AppHandle;
use zip::write::SimpleFileOptions;

use crate::events::{self, log, progress};

const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "bmp", "gif", "tif", "tiff", "avif",
];

/// 프론트엔드 옵션과 1:1 대응 (camelCase).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessOptions {
    pub use_waifu2x: bool,     // false면 엔진 바이패스(변환/재패키징만)
    pub use_gpu: bool,         // true면 ncnn(Vulkan) 배치, 아니면 CPU(ort)
    pub upscale: String,       // "none" | "2x" | "4x"
    pub denoise_level: String, // "none" | "0".."3"
    pub resize_to_original: bool, // 처리 후 원본 크기로 축소(supersampling: 노이즈↓·용량↓)
    pub whiten: bool,
    pub white_strength: u8, // 0..100
    pub keep_color: bool,
    pub format: String, // "cbz" | "pdf" | "folder"
    pub quality: u8,    // 40..100
    pub output_dir: String,
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// 한 페이지의 원본 위치(파일 또는 아카이브 내부 엔트리).
#[derive(Clone)]
enum PageRef {
    File(PathBuf),
    Zip {
        archive: PathBuf,
        index: usize,
        name: String,
    },
}

/// 스프레드 분할 결과 — 원본에서 어느 부분을 쓸지.
#[derive(Clone, Copy, PartialEq)]
enum Crop {
    Whole,
    Left,
    Right,
}

/// 처리 단위: 원본 페이지(PageRef)에서 잘라낼 영역(Crop) 하나.
/// 분할이 켜지면 스프레드 한 장이 Left/Right 두 유닛으로 늘어난다.
#[derive(Clone)]
struct PageUnit {
    src: PageRef,
    crop: Crop,
}

impl PageUnit {
    /// 원본을 로드하고 crop을 적용한 RGB 이미지를 반환.
    fn load(&self) -> Result<RgbImage, String> {
        let img = self.src.load()?.to_rgb8();
        Ok(apply_crop(img, self.crop))
    }

    /// 로그·표시용 이름. 분할된 유닛은 원본 이름에 좌/우 표시를 덧붙인다.
    fn name(&self) -> String {
        let base = self.src.name();
        match self.crop {
            Crop::Whole => base,
            Crop::Left => format!("{base} (L)"),
            Crop::Right => format!("{base} (R)"),
        }
    }
}

/// crop 영역을 잘라 반환. Whole이면 원본 그대로.
fn apply_crop(img: RgbImage, crop: Crop) -> RgbImage {
    let (w, h) = (img.width(), img.height());
    match crop {
        Crop::Whole => img,
        Crop::Left => image::imageops::crop_imm(&img, 0, 0, w / 2, h).to_image(),
        Crop::Right => {
            let mid = w / 2;
            image::imageops::crop_imm(&img, mid, 0, w - mid, h).to_image()
        }
    }
}

/// crop을 참조로 적용. Whole은 원본을 빌려 복제를 피하고, 분할만 새 버퍼를 만든다.
fn apply_crop_ref(img: &RgbImage, crop: Crop) -> std::borrow::Cow<'_, RgbImage> {
    use std::borrow::Cow;
    let (w, h) = (img.width(), img.height());
    match crop {
        Crop::Whole => Cow::Borrowed(img),
        Crop::Left => Cow::Owned(image::imageops::crop_imm(img, 0, 0, w / 2, h).to_image()),
        Crop::Right => {
            let mid = w / 2;
            Cow::Owned(image::imageops::crop_imm(img, mid, 0, w - mid, h).to_image())
        }
    }
}

impl PageRef {
    fn name(&self) -> String {
        match self {
            PageRef::File(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            PageRef::Zip { name, .. } => name.clone(),
        }
    }

    fn load(&self) -> Result<DynamicImage, String> {
        match self {
            PageRef::File(p) => image::open(p).map_err(|e| format!("{}: {e}", p.display())),
            PageRef::Zip {
                archive,
                index,
                name,
            } => {
                let f = fs::File::open(archive).map_err(|e| e.to_string())?;
                let mut ar = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
                let mut zf = ar.by_index(*index).map_err(|e| e.to_string())?;
                let mut bytes = Vec::new();
                zf.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                image::load_from_memory(&bytes).map_err(|e| format!("{name}: {e}"))
            }
        }
    }
}

/// 하나의 소스 경로(파일/폴더/아카이브)를 페이지 목록으로 확장.
fn collect_pages(path: &Path, app: &AppHandle) -> Vec<PageRef> {
    if path.is_dir() {
        let mut files: Vec<PathBuf> = match fs::read_dir(path) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| is_image(p))
                .collect(),
            Err(e) => {
                log(app, "warn", format!("{}: {e}", path.display()));
                Vec::new()
            }
        };
        files.sort();
        return files.into_iter().map(PageRef::File).collect();
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "cbz" | "zip" => {
            let f = match fs::File::open(path) {
                Ok(f) => f,
                Err(e) => {
                    log(app, "warn", format!("{}: {e}", path.display()));
                    return Vec::new();
                }
            };
            let mut ar = match zip::ZipArchive::new(f) {
                Ok(a) => a,
                Err(e) => {
                    log(app, "warn", format!("{}: {e}", path.display()));
                    return Vec::new();
                }
            };
            let mut entries: Vec<(usize, String)> = Vec::new();
            for i in 0..ar.len() {
                if let Ok(zf) = ar.by_index(i) {
                    if zf.is_file() {
                        let name = zf.name().to_string();
                        if is_image(Path::new(&name)) {
                            entries.push((i, name));
                        }
                    }
                }
            }
            entries.sort_by(|a, b| a.1.cmp(&b.1));
            entries
                .into_iter()
                .map(|(index, name)| PageRef::Zip {
                    archive: path.to_path_buf(),
                    index,
                    name,
                })
                .collect()
        }
        "pdf" => crate::pdf::extract_pdf(path, app)
            .into_iter()
            .map(PageRef::File)
            .collect(),
        e if IMAGE_EXTS.contains(&e) => vec![PageRef::File(path.to_path_buf())],
        _ => {
            log(app, "warn", format!("지원하지 않는 형식: {}", path.display()));
            Vec::new()
        }
    }
}

/// 스캔 종이색을 흰색으로. 채널별 밝은 쪽 퍼센타일을 화이트포인트로 잡아 스트레치.
fn whiten(img: &mut RgbImage, strength: u8, keep_color: bool) {
    let total = img.width() as u64 * img.height() as u64;
    if total == 0 {
        return;
    }
    // strength 0 → 100%(거의 변화 없음), 100 → 80%(공격적).
    let p = 100.0 - (strength as f32) * 0.20;

    let mut hist = [[0u32; 256]; 3];
    for px in img.pixels() {
        hist[0][px[0] as usize] += 1;
        hist[1][px[1] as usize] += 1;
        hist[2][px[2] as usize] += 1;
    }

    let target = (total as f32 * p / 100.0) as u32;
    let mut wp = [255u8; 3];
    for c in 0..3 {
        let mut acc = 0u32;
        for v in 0..256usize {
            acc += hist[c][v];
            if acc >= target {
                wp[c] = v.max(1) as u8;
                break;
            }
        }
    }

    let scale = |v: f32, w: u8| -> u8 { (v * 255.0 / w as f32).round().min(255.0) as u8 };

    if keep_color {
        for px in img.pixels_mut() {
            px[0] = scale(px[0] as f32, wp[0]);
            px[1] = scale(px[1] as f32, wp[1]);
            px[2] = scale(px[2] as f32, wp[2]);
        }
    } else {
        let w = *wp.iter().max().unwrap();
        for px in img.pixels_mut() {
            let lum = 0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32;
            let g = scale(lum, w);
            px[0] = g;
            px[1] = g;
            px[2] = g;
        }
    }
}

/// 페이지 크기와 읽기 방향으로 분할 결과(Crop 순서)를 정한다.
/// 가로가 세로보다 긴 페이지(스프레드)만 좌/우로 나누고, 세로 단면은 Whole 하나.
/// direction "rl"(우철)이면 오른쪽 절반이 먼저(작은 페이지 번호), "lr"이면 왼쪽이 먼저.
fn plan_crops(w: u32, h: u32, direction: &str) -> Vec<Crop> {
    if w <= h {
        return vec![Crop::Whole];
    }
    if direction == "lr" {
        vec![Crop::Left, Crop::Right]
    } else {
        vec![Crop::Right, Crop::Left]
    }
}

/// 결과를 원본 크기(ow×oh)로 축소. 이미 같거나 작으면 그대로 둔다.
/// Lanczos3로 다운샘플하면 업스케일 때 남은 잔여 노이즈가 평균화(supersampling)돼
/// 화질은 유지하면서 고주파가 줄어 파일 크기가 작아진다.
fn resize_to(img: RgbImage, ow: u32, oh: u32) -> RgbImage {
    if img.width() <= ow || img.height() <= oh {
        return img;
    }
    image::imageops::resize(&img, ow, oh, image::imageops::FilterType::Lanczos3)
}

fn encode(img: &RgbImage, quality: u8) -> Result<(Vec<u8>, &'static str), String> {
    let mut cur = Cursor::new(Vec::new());
    if quality >= 100 {
        image::codecs::png::PngEncoder::new(&mut cur)
            .write_image(img.as_raw(), img.width(), img.height(), ExtendedColorType::Rgb8)
            .map_err(|e| e.to_string())?;
        Ok((cur.into_inner(), "png"))
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cur, quality)
            .write_image(img.as_raw(), img.width(), img.height(), ExtendedColorType::Rgb8)
            .map_err(|e| e.to_string())?;
        Ok((cur.into_inner(), "jpg"))
    }
}

/// 출력 폴더가 지정되지 않았을 때 입력 소스 위치를 기준으로 결정.
/// 폴더/파일 모두 그 부모 디렉터리에 결과물을 만들어 원본 옆에 남긴다.
fn default_output_dir(sources: &[String]) -> PathBuf {
    let first = Path::new(&sources[0]);
    first
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn job_name(sources: &[String]) -> String {
    sources
        .first()
        .and_then(|s| {
            Path::new(s)
                .file_stem()
                .map(|x| x.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "output".to_string())
}

/// 실행 중 취소 플래그(관리 상태). 여러 작업이 동시에 돌지 않는다는 가정.
#[derive(Default)]
pub struct CancelFlag(pub Arc<AtomicBool>);

/// 결과물 출력 대상. CPU·GPU 경로가 공유한다.
enum Sink {
    Folder(PathBuf),
    Zip(zip::ZipWriter<fs::File>),
}

impl Sink {
    fn new(fmt: &str, out_dir: &Path, job: &str) -> Result<Self, String> {
        if fmt == "folder" {
            let d = out_dir.join(job);
            fs::create_dir_all(&d).map_err(|e| e.to_string())?;
            Ok(Sink::Folder(d))
        } else {
            let f = fs::File::create(out_dir.join(format!("{job}.cbz")))
                .map_err(|e| e.to_string())?;
            Ok(Sink::Zip(zip::ZipWriter::new(f)))
        }
    }

    fn write(&mut self, fname: &str, bytes: &[u8]) -> Result<(), String> {
        match self {
            Sink::Folder(dir) => fs::write(dir.join(fname), bytes).map_err(|e| e.to_string()),
            Sink::Zip(zw) => {
                zw.start_file(
                    fname,
                    SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Stored),
                )
                .map_err(|e| e.to_string())?;
                zw.write_all(bytes).map_err(|e| e.to_string())
            }
        }
    }

    fn finish(self) -> Result<(), String> {
        if let Sink::Zip(zw) = self {
            zw.finish().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn run_inner(
    app: &AppHandle,
    pages: &[PageUnit],
    sources: &[String],
    opts: &ProcessOptions,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if pages.is_empty() {
        return Err("처리할 이미지가 없습니다".to_string());
    }
    let total = pages.len();
    log(app, "info", format!("총 {total}페이지 처리 시작"));

    let out_dir = if opts.output_dir.trim().is_empty() {
        default_output_dir(sources)
    } else {
        PathBuf::from(opts.output_dir.trim())
    };
    let out_dir = out_dir.as_path();
    fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let job = job_name(sources);

    let mut fmt = opts.format.as_str();
    if fmt == "pdf" {
        log(app, "warn", "PDF 출력은 아직 미지원 — CBZ로 저장합니다");
        fmt = "cbz";
    }

    // GPU(ncnn/Vulkan) 경로 사용 여부. waifu2x가 켜져 있고, 실제 처리가 필요하고,
    // 번들 바이너리가 있을 때만. 요청했지만 불가하면 CPU로 폴백(경고).
    let want_gpu = opts.use_gpu && opts.use_waifu2x;
    let gpu = want_gpu
        && crate::ncnn::args_for(opts).is_some()
        && {
            let ok = crate::ncnn::is_available(app);
            if !ok {
                log(app, "warn", "GPU(ncnn) 바이너리를 찾지 못해 CPU로 폴백합니다");
            }
            ok
        };

    let mut sink = Sink::new(fmt, out_dir, &job)?;

    if gpu {
        run_gpu(app, &pages, opts, &mut sink, cancel)?;
    } else {
        run_cpu(app, &pages, opts, &mut sink, cancel)?;
    }

    sink.finish()?;

    if cancel.load(Ordering::Relaxed) {
        log(app, "warn", format!("중지됨 (부분 저장 → {})", out_dir.display()));
    } else {
        log(app, "success", format!("완료 → {}", out_dir.display()));
    }
    Ok(())
}

/// 한 페이지 유닛을 처리해 (bytes, ext)를 반환한다.
/// 순서: load(+crop) → 추론(노이즈 제거·업스케일) → 원본 크기로 축소 → 화이트닝 → 인코딩.
/// 화이트닝을 맨 뒤에 두는 이유: 화이트닝은 콘트라스트 스트레치라 스캔 그레인을 증폭하므로,
/// 먼저 waifu2x로 노이즈를 정리한 뒤 가장 깨끗한 이미지에 적용해야 배경이 깔끔해진다.
/// 엔진이 None이면 추론을 건너뛴다(변환/재패키징만).
fn process_page(
    unit: &PageUnit,
    opts: &ProcessOptions,
    engine: Option<&mut crate::waifu2x::Engine>,
) -> Result<(Vec<u8>, &'static str), String> {
    let rgb = unit.load()?;
    let (ow, oh) = (rgb.width(), rgb.height());
    let mut out = match engine {
        Some(e) => e.process(&rgb)?,
        None => rgb,
    };
    if opts.resize_to_original {
        out = resize_to(out, ow, oh);
    }
    if opts.whiten {
        whiten(&mut out, opts.white_strength, opts.keep_color);
    }
    encode(&out, opts.quality)
}

/// 워커 수 × 세션당 op 스레드 수를 코어 수에 맞춰 배분.
/// 페이지 병렬(워커)과 op 병렬(intra)을 곱했을 때 논리코어를 넘지 않게 한다.
/// waifu2x가 없으면(변환만) 추론이 없어 op 스레드는 무의미 → 워커만 많이.
fn plan_pool(pages: usize, needs_engine: bool) -> (usize, usize) {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    if !needs_engine {
        return (cores.min(pages).max(1), 0);
    }
    // conv 추론은 SMT 이득이 없고 메모리 대역폭이 병목 → 물리코어(논리/2) 기준.
    // 워커 하나 = whole-image 추론 하나 = 세션당 피크 수 GB(중간 feature map).
    // 워커를 늘리면 메모리가 그 배수로 뛰어 페이징으로 정지 수준이 됨 → 2개 상한.
    // 대신 세션당 op 스레드를 크게(8) 줘서 코어를 채운다.
    let phys = (cores / 2).max(1);
    let workers = 2.min(pages.max(1)).min(phys);
    let intra = (phys / workers).clamp(1, 8);
    (workers, intra)
}

/// CPU 경로: 세션 풀로 페이지를 병렬 처리하고, 결과는 인덱스 순서대로 싱크에 기록.
fn run_cpu(
    app: &AppHandle,
    pages: &[PageUnit],
    opts: &ProcessOptions,
    sink: &mut Sink,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let total = pages.len();
    let needs_engine = crate::waifu2x::is_needed(opts);
    if !needs_engine {
        log(app, "info", "업스케일·노이즈 제거 없음 — 해상도 유지");
    }

    let (workers, intra) = plan_pool(total, needs_engine);
    log(
        app,
        "info",
        format!("CPU 병렬: 워커 {workers} × op 스레드 {}", if intra == 0 { "기본".to_string() } else { intra.to_string() }),
    );

    // 다음 처리할 페이지 인덱스(워커들이 경합적으로 가져감).
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    // (인덱스, 결과) 채널. 결과는 순서 무관하게 도착 → 수집 스레드가 재정렬.
    #[allow(clippy::type_complexity)]
    let (tx, rx) =
        mpsc::channel::<(usize, Result<(Vec<u8>, &'static str), String>)>();

    let result: Result<(), String> = std::thread::scope(|scope| {
        // 워커 스레드들: 각자 엔진 하나를 소유하고 페이지를 집어 처리.
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            let done = &done;
            let cancel = &*cancel;
            scope.spawn(move || {
                let mut engine = match crate::waifu2x::Engine::new(app, opts, intra, true) {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = tx.send((usize::MAX, Err(format!("엔진 초기화 실패: {e}"))));
                        return;
                    }
                };
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= total {
                        break;
                    }
                    log(app, "info", format!("[{}/{}] {}", i + 1, total, pages[i].name()));
                    let t = std::time::Instant::now();
                    let r = process_page(&pages[i], opts, engine.as_mut());
                    log(
                        app,
                        "info",
                        format!("[{}/{}] 완료 ({:.1}s)", i + 1, total, t.elapsed().as_secs_f64()),
                    );
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    progress(app, n, total);
                    if tx.send((i, r)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx); // 워커들의 클론만 남게 → 다 끝나면 rx가 닫힘.

        // 수집: 순서 무관 도착을 버퍼에 모아 인덱스 순서대로 sink에 기록.
        let mut pending: HashMap<usize, (Vec<u8>, &'static str)> = HashMap::new();
        let mut want = 0usize;
        for (i, r) in rx {
            match r {
                Ok(v) => {
                    pending.insert(i, v);
                }
                Err(e) => {
                    cancel.store(true, Ordering::Relaxed);
                    return Err(e);
                }
            }
            // 연속으로 준비된 것부터 흘려보냄(메모리 최소화).
            while let Some((bytes, ext)) = pending.remove(&want) {
                sink.write(&format!("page_{:04}.{}", want + 1, ext), &bytes)?;
                want += 1;
            }
        }
        // 남은 것 정리(취소 등으로 중간이 비어도 있는 것만 순서대로).
        let mut keys: Vec<usize> = pending.keys().copied().collect();
        keys.sort_unstable();
        for k in keys {
            if k == want {
                let (bytes, ext) = pending.remove(&k).unwrap();
                sink.write(&format!("page_{:04}.{}", k + 1, ext), &bytes)?;
                want += 1;
            }
        }
        Ok(())
    });

    if cancel.load(Ordering::Relaxed) && result.is_ok() {
        log(app, "warn", "사용자에 의해 중지됨");
    }
    result
}

/// GPU 경로: 페이지를 임시 폴더에 PNG로 스테이징(crop 적용) → ncnn 배치 업스케일 →
/// 결과를 축소·화이트닝 후 품질 옵션으로 재인코딩해 싱크에 기록.
/// 화이트닝은 CPU 경로와 마찬가지로 업스케일·축소 뒤(재인코딩 단계)에 적용한다.
fn run_gpu(
    app: &AppHandle,
    pages: &[PageUnit],
    opts: &ProcessOptions,
    sink: &mut Sink,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let total = pages.len();
    let (scale, noise) = crate::ncnn::args_for(opts).ok_or("GPU: 처리할 작업 없음")?;

    // 임시 입력/출력 폴더(작업 종료 시 정리). 인덱스 순서를 파일명으로 보존.
    let tmp = std::env::temp_dir().join(format!("comicv_gpu_{}", std::process::id()));
    let in_dir = tmp.join("in");
    let out_dir = tmp.join("out");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&in_dir).map_err(|e| e.to_string())?;
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    // 1) 스테이징: 원본 로드 + crop → PNG 저장. 원본 크기는 축소용으로 보관.
    // (분할은 prepare 단계에서 PageUnit으로 확정됨 → 여기서는 유닛 하나 = 파일 하나.)
    // 로드·PNG 인코딩·디스크 쓰기는 페이지마다 독립적이라 청크 단위로 병렬 처리한다.
    // 청크 경계에서 취소를 확인하고, orig_sizes는 청크 순서대로 이어붙여 정합성을 지킨다.
    use rayon::prelude::*;
    const STAGE_CHUNK: usize = 32;
    let t_stage = std::time::Instant::now();
    let mut orig_sizes: Vec<(u32, u32)> = Vec::with_capacity(total);
    for (c, chunk) in pages.chunks(STAGE_CHUNK).enumerate() {
        if cancel.load(Ordering::Relaxed) {
            log(app, "warn", "사용자에 의해 중지됨");
            let _ = fs::remove_dir_all(&tmp);
            return Ok(());
        }
        let base = c * STAGE_CHUNK;
        let sizes: Result<Vec<(u32, u32)>, String> = chunk
            .par_iter()
            .enumerate()
            .map(|(k, page)| {
                let i = base + k;
                let rgb = page.load()?;
                let size = (rgb.width(), rgb.height());
                let (bytes, _) = encode(&rgb, 100)?; // 중간물은 무손실 PNG
                fs::write(in_dir.join(format!("page_{:04}.png", i + 1)), &bytes)
                    .map_err(|e| e.to_string())?;
                Ok(size)
            })
            .collect();
        orig_sizes.extend(sizes?);
    }
    let stage_s = t_stage.elapsed().as_secs_f64();
    log(
        app,
        "info",
        format!("스테이징 {total}p 완료 ({stage_s:.1}s, {:.2}s/p) — GPU 업스케일 시작", stage_s / total as f64),
    );

    // 2) ncnn 배치 업스케일. 진행률은 출력 파일 수로 추정(전체의 대부분을 차지).
    let t_infer = std::time::Instant::now();
    let batch = crate::ncnn::run_batch(app, &in_dir, &out_dir, scale, noise, cancel, |done| {
        progress(app, done.min(total), total);
    });
    if let Err(e) = batch {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if cancel.load(Ordering::Relaxed) {
        let _ = fs::remove_dir_all(&tmp);
        return Ok(());
    }
    let infer_s = t_infer.elapsed().as_secs_f64();
    log(
        app,
        "info",
        format!("GPU 추론 완료 ({infer_s:.1}s, {:.2}s/p)", infer_s / total as f64),
    );

    // 3) 재인코딩: ncnn 결과 PNG를 품질 옵션대로 저장(입력과 동일 파일명 → 순서 보존).
    // 디코드·Lanczos3 축소·화이트닝·인코딩은 페이지마다 무겁고 독립적이라 청크 단위로
    // 병렬 처리한다. 업스케일 결과는 이미지가 크므로 청크로 끊어 메모리 피크를 억제하고,
    // 인코딩된 bytes만 청크 순서대로 싱크에 기록해 페이지 순서를 보존한다.
    let t_enc = std::time::Instant::now();
    const ENC_CHUNK: usize = 16;
    for chunk_start in (0..total).step_by(ENC_CHUNK) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let end = (chunk_start + ENC_CHUNK).min(total);
        // 청크 내 페이지를 병렬로 최종 이미지 bytes까지 만든다(각 원소는 Option — 누락은 None).
        let encoded: Vec<Option<(Vec<u8>, &'static str)>> = (chunk_start..end)
            .into_par_iter()
            .map(|i| {
                let src = out_dir.join(format!("page_{:04}.png", i + 1));
                let mut img = match image::open(&src) {
                    Ok(im) => im.to_rgb8(),
                    Err(e) => {
                        log(app, "warn", format!("GPU 결과 누락: {} ({e})", src.display()));
                        return None;
                    }
                };
                if opts.resize_to_original {
                    let (ow, oh) = orig_sizes[i];
                    img = resize_to(img, ow, oh);
                }
                // 화이트닝은 업스케일·축소 뒤(가장 깨끗한 최종 이미지)에 적용.
                if opts.whiten {
                    whiten(&mut img, opts.white_strength, opts.keep_color);
                }
                encode(&img, opts.quality).ok()
            })
            .collect();
        // 순서대로 싱크에 기록(싱크는 단일 스레드 소유).
        for (k, item) in encoded.into_iter().enumerate() {
            if let Some((bytes, ext)) = item {
                let i = chunk_start + k;
                sink.write(&format!("page_{:04}.{}", i + 1, ext), &bytes)?;
            }
        }
    }
    let enc_s = t_enc.elapsed().as_secs_f64();
    log(
        app,
        "info",
        format!("재인코딩 완료 ({enc_s:.1}s, {:.2}s/p)", enc_s / total as f64),
    );

    let _ = fs::remove_dir_all(&tmp);
    Ok(())
}

fn run(
    app: AppHandle,
    pages: Vec<PageUnit>,
    sources: Vec<String>,
    opts: ProcessOptions,
    cancel: Arc<AtomicBool>,
) {
    if let Err(e) = run_inner(&app, &pages, &sources, &opts, &cancel) {
        log(&app, "error", format!("처리 실패: {e}"));
    }
    events::done(&app);
}

/// prepare 단계에서 확정한 페이지 유닛 목록을 소스와 함께 캐시하는 관리 상태.
/// 프론트엔드는 이 목록의 id로 선택/해제하고, start 시 선택된 id만 처리한다.
#[derive(Default)]
pub struct PreparedPages(pub Mutex<Prepared>);

#[derive(Default)]
pub struct Prepared {
    pub units: Vec<PageUnit>,
    pub sources: Vec<String>,
}

/// 프론트엔드로 보내는 페이지 프리뷰 하나(썸네일 data URL 포함).
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PagePreview {
    pub id: usize,       // units 벡터 내 인덱스
    pub name: String,    // 표시용 이름(분할 시 L/R 표시)
    pub thumb: String,   // data:image/jpeg;base64,... 썸네일
    pub width: u32,      // crop 적용 후 원본 픽셀 크기
    pub height: u32,
}

const THUMB_MAX: u32 = 320; // 썸네일 긴 변 최대 픽셀

/// 썸네일을 만들어 data URL(JPEG q70)로 인코딩.
fn make_thumb(img: &RgbImage) -> Result<String, String> {
    use base64::Engine as _;
    let (w, h) = (img.width(), img.height());
    let thumb = if w.max(h) > THUMB_MAX {
        let (tw, th) = if w >= h {
            (THUMB_MAX, (h * THUMB_MAX / w).max(1))
        } else {
            ((w * THUMB_MAX / h).max(1), THUMB_MAX)
        };
        image::imageops::resize(img, tw, th, image::imageops::FilterType::Triangle)
    } else {
        img.clone()
    };
    let mut cur = Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cur, 70)
        .write_image(thumb.as_raw(), thumb.width(), thumb.height(), ExtendedColorType::Rgb8)
        .map_err(|e| e.to_string())?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(cur.into_inner());
    Ok(format!("data:image/jpeg;base64,{b64}"))
}

/// 소스들을 수집·분할해 페이지 유닛을 확정하고, 각 유닛의 썸네일 프리뷰를 반환한다.
/// 결과 유닛 목록은 state에 캐시되어 이후 start_processing이 id로 참조한다.
/// split_pages가 켜지면 가로가 긴 스프레드를 좌/우 두 유닛으로 나눈다.
#[tauri::command]
pub async fn prepare_pages(
    app: AppHandle,
    state: tauri::State<'_, PreparedPages>,
    sources: Vec<String>,
    split_pages: bool,
    split_direction: String,
) -> Result<Vec<PagePreview>, String> {
    if sources.is_empty() {
        return Err("추가된 소스가 없습니다".to_string());
    }

    // 디코드·리사이즈·JPEG 인코딩은 CPU 바운드라 UI(메인) 스레드에서 돌면 앱이 얼어붙는다.
    // 블로킹 풀에서 rayon으로 페이지를 병렬 처리해 프리즈를 없애고 준비 시간을 단축한다.
    let sources2 = sources.clone();
    let dir = split_direction.clone();
    let (units, previews) = tauri::async_runtime::spawn_blocking(move || {
        prepare_pages_blocking(&app, &sources2, split_pages, &dir)
    })
    .await
    .map_err(|e| e.to_string())??;

    // state에 캐시.
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    guard.units = units;
    guard.sources = sources;

    Ok(previews)
}

/// prepare의 CPU 바운드 본체. 블로킹 스레드에서 호출된다.
/// 소스를 페이지 참조로 확장한 뒤, 각 참조를 rayon으로 병렬 로드·분할·썸네일화한다.
/// 페이지 순서를 보존하기 위해 참조별 결과를 순서대로 이어붙이고 id를 재부여한다.
fn prepare_pages_blocking(
    app: &AppHandle,
    sources: &[String],
    split_pages: bool,
    split_direction: &str,
) -> Result<(Vec<PageUnit>, Vec<PagePreview>), String> {
    use rayon::prelude::*;

    // 1) 소스 → 원본 페이지 참조 목록.
    let mut refs: Vec<PageRef> = Vec::new();
    for s in sources {
        refs.append(&mut collect_pages(Path::new(s), app));
    }
    if refs.is_empty() {
        return Err("처리할 이미지가 없습니다".to_string());
    }

    // 2) 참조를 병렬로 로드·분할·썸네일화. 각 참조가 (유닛, 썸네일, w, h) 목록을 낸다.
    //    id는 순서 확정 후 부여하므로 여기선 비운다.
    let per_ref: Vec<Vec<(PageUnit, String, u32, u32)>> = refs
        .par_iter()
        .map(|r| {
            let img = match r.load() {
                Ok(im) => im.to_rgb8(),
                Err(e) => {
                    log(app, "warn", format!("프리뷰 로드 실패: {e}"));
                    return Vec::new();
                }
            };
            let crops = if split_pages {
                plan_crops(img.width(), img.height(), split_direction)
            } else {
                vec![Crop::Whole]
            };
            let mut out = Vec::with_capacity(crops.len());
            for crop in &crops {
                let unit = PageUnit {
                    src: r.clone(),
                    crop: *crop,
                };
                // 참조 뷰로 crop → Whole은 원본 참조를 그대로 인코딩(복제 없음),
                // 분할은 절반 영역만 새 버퍼로 뽑는다.
                let cropped = apply_crop_ref(&img, *crop);
                let (w, h) = (cropped.width(), cropped.height());
                match make_thumb(&cropped) {
                    // (&Cow는 Deref로 &RgbImage에 맞춰짐)
                    Ok(thumb) => out.push((unit, thumb, w, h)),
                    Err(e) => log(app, "warn", format!("썸네일 생성 실패: {e}")),
                }
            }
            out
        })
        .collect();

    // 3) 순서대로 이어붙이며 id 부여.
    let mut units: Vec<PageUnit> = Vec::new();
    let mut previews: Vec<PagePreview> = Vec::new();
    for group in per_ref {
        for (unit, thumb, w, h) in group {
            let id = units.len();
            previews.push(PagePreview {
                id,
                name: unit.name(),
                thumb,
                width: w,
                height: h,
            });
            units.push(unit);
        }
    }

    log(app, "info", format!("{}페이지 준비 완료", units.len()));
    Ok((units, previews))
}

#[tauri::command]
pub fn start_processing(
    app: AppHandle,
    cancel_state: tauri::State<'_, CancelFlag>,
    prepared: tauri::State<'_, PreparedPages>,
    selected_ids: Vec<usize>,
    options: ProcessOptions,
) -> Result<(), String> {
    // 선택된 id만, 준비된 순서를 유지해 처리 목록을 만든다.
    let (pages, sources) = {
        let guard = prepared.0.lock().map_err(|e| e.to_string())?;
        if guard.units.is_empty() {
            return Err("먼저 페이지를 준비하세요".to_string());
        }
        let wanted: std::collections::HashSet<usize> = selected_ids.into_iter().collect();
        let pages: Vec<PageUnit> = guard
            .units
            .iter()
            .enumerate()
            .filter(|(i, _)| wanted.contains(i))
            .map(|(_, u)| u.clone())
            .collect();
        (pages, guard.sources.clone())
    };

    if pages.is_empty() {
        return Err("선택된 페이지가 없습니다".to_string());
    }

    cancel_state.0.store(false, Ordering::Relaxed);
    let cancel = Arc::clone(&cancel_state.0);
    std::thread::spawn(move || run(app, pages, sources, options, cancel));
    Ok(())
}

#[tauri::command]
pub fn cancel_processing(state: tauri::State<'_, CancelFlag>) {
    state.0.store(true, Ordering::Relaxed);
}
