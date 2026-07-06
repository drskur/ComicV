use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use image::{DynamicImage, ExtendedColorType, ImageEncoder, RgbImage};
use tauri::{AppHandle, Emitter};
use zip::write::SimpleFileOptions;

const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "bmp", "gif", "tif", "tiff", "avif",
];

/// 프론트엔드 옵션과 1:1 대응 (camelCase).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessOptions {
    pub upscale: String,       // "none" | "2x" | "4x"
    pub denoise_level: String, // "none" | "0".."3"
    pub whiten: bool,
    pub white_strength: u8, // 0..100
    pub keep_color: bool,
    pub format: String, // "cbz" | "pdf" | "folder"
    pub quality: u8,    // 40..100
    pub output_dir: String,
}

#[derive(Clone, serde::Serialize)]
struct LogEvent {
    level: String,
    message: String,
}

#[derive(Clone, serde::Serialize)]
struct ProgressEvent {
    current: usize,
    total: usize,
    percent: u32,
}

fn log(app: &AppHandle, level: &str, message: impl Into<String>) {
    let _ = app.emit(
        "process://log",
        LogEvent {
            level: level.to_string(),
            message: message.into(),
        },
    );
}

fn progress(app: &AppHandle, current: usize, total: usize) {
    let percent = if total == 0 {
        0
    } else {
        (current * 100 / total) as u32
    };
    let _ = app.emit(
        "process://progress",
        ProgressEvent {
            current,
            total,
            percent,
        },
    );
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// 한 페이지의 원본 위치(파일 또는 아카이브 내부 엔트리).
enum PageRef {
    File(PathBuf),
    Zip {
        archive: PathBuf,
        index: usize,
        name: String,
    },
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
        "pdf" => {
            log(
                app,
                "warn",
                format!("PDF 입력은 아직 미지원(렌더러 필요): {}", path.display()),
            );
            Vec::new()
        }
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

fn run_inner(app: &AppHandle, sources: &[String], opts: &ProcessOptions) -> Result<(), String> {
    let mut pages: Vec<PageRef> = Vec::new();
    for s in sources {
        pages.append(&mut collect_pages(Path::new(s), app));
    }
    if pages.is_empty() {
        return Err("처리할 이미지가 없습니다".to_string());
    }
    let total = pages.len();
    log(app, "info", format!("총 {total}페이지 처리 시작"));

    let mut engine = crate::waifu2x::Engine::new(app, opts).map_err(|e| format!("엔진 초기화 실패: {e}"))?;
    if engine.is_none() {
        log(app, "info", "업스케일·노이즈 제거 없음 — 해상도 유지");
    }

    let out_dir = Path::new(&opts.output_dir);
    fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let job = job_name(sources);

    let mut fmt = opts.format.as_str();
    if fmt == "pdf" {
        log(app, "warn", "PDF 출력은 아직 미지원 — CBZ로 저장합니다");
        fmt = "cbz";
    }

    // 출력 싱크 준비
    let folder_out = if fmt == "folder" {
        let d = out_dir.join(&job);
        fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        Some(d)
    } else {
        None
    };
    let mut zip_out = if fmt == "cbz" {
        let f = fs::File::create(out_dir.join(format!("{job}.cbz"))).map_err(|e| e.to_string())?;
        Some(zip::ZipWriter::new(f))
    } else {
        None
    };

    for (i, page) in pages.iter().enumerate() {
        log(app, "info", format!("[{}/{}] {}", i + 1, total, page.name()));

        let mut rgb = page.load()?.to_rgb8();
        if opts.whiten {
            whiten(&mut rgb, opts.white_strength, opts.keep_color);
        }
        let out = match &mut engine {
            Some(e) => e.process(&rgb, app)?,
            None => rgb,
        };
        let (bytes, ext) = encode(&out, opts.quality)?;
        let fname = format!("page_{:04}.{}", i + 1, ext);

        if let Some(dir) = &folder_out {
            fs::write(dir.join(&fname), &bytes).map_err(|e| e.to_string())?;
        } else if let Some(zw) = &mut zip_out {
            zw.start_file(
                &fname,
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .map_err(|e| e.to_string())?;
            zw.write_all(&bytes).map_err(|e| e.to_string())?;
        }

        progress(app, i + 1, total);
    }

    if let Some(zw) = zip_out.take() {
        zw.finish().map_err(|e| e.to_string())?;
    }

    log(app, "success", format!("완료 → {}", out_dir.display()));
    Ok(())
}

fn run(app: AppHandle, sources: Vec<String>, opts: ProcessOptions) {
    if let Err(e) = run_inner(&app, &sources, &opts) {
        log(&app, "error", format!("처리 실패: {e}"));
    }
    let _ = app.emit("process://done", ());
}

#[tauri::command]
pub fn start_processing(
    app: AppHandle,
    sources: Vec<String>,
    options: ProcessOptions,
) -> Result<(), String> {
    if sources.is_empty() {
        return Err("추가된 소스가 없습니다".to_string());
    }
    if options.output_dir.trim().is_empty() {
        return Err("출력 경로를 지정하세요".to_string());
    }
    std::thread::spawn(move || run(app, sources, options));
    Ok(())
}
