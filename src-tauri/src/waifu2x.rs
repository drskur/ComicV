//! waifu2x(cunet) ONNX 추론 — ort(CPU 전용).
//!
//! GPU 가속은 별도 ncnn(Vulkan) 백엔드(crate::ncnn)가 담당한다. DirectML은
//! 대상 기기(AMD Strix Halo iGPU)에서 출력이 0으로 붕괴해 제거했다.
//!
//! 모델은 `resources/models/cunet/*.onnx` (deepghs/waifu2x_onnx, nunif 기반).
//! 노이즈 레벨(-n)과 배율(-s) 조합으로 모델 파일을 고른다.
//!  - 1x + denoise n     → noise{n}.onnx
//!  - 2x (no denoise)    → scale2x.onnx
//!  - 2x + denoise n     → noise{n}_scale2x.onnx
//!  - 4x                 → 위 2x 패스 후 scale2x.onnx 한 번 더

use std::path::PathBuf;

use image::{Rgb, RgbImage};
use ort::session::Session;
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::events::log;
use crate::pipeline::ProcessOptions;

fn denoise_opt(s: &str) -> Option<u8> {
    match s {
        "0" => Some(0),
        "1" => Some(1),
        "2" => Some(2),
        "3" => Some(3),
        _ => None,
    }
}

fn base_model(two_x: bool, denoise: Option<u8>) -> Option<String> {
    match (two_x, denoise) {
        (false, None) => None,
        (false, Some(n)) => Some(format!("noise{n}.onnx")),
        (true, None) => Some("scale2x.onnx".to_string()),
        (true, Some(n)) => Some(format!("noise{n}_scale2x.onnx")),
    }
}

fn model_path(app: &AppHandle, name: &str) -> PathBuf {
    if let Ok(p) = app
        .path()
        .resolve(format!("resources/models/cunet/{name}"), BaseDirectory::Resource)
    {
        if p.exists() {
            return p;
        }
    }
    // 개발 모드 폴백
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("models")
        .join("cunet")
        .join(name)
}

fn load_session(path: &PathBuf, intra_threads: usize) -> Result<Session, String> {
    if !path.exists() {
        return Err(format!("모델 파일 없음: {}", path.display()));
    }
    // CPU 전용 경로. GPU 가속은 별도 ncnn(Vulkan) 백엔드가 담당(crate::ncnn).
    // DirectML은 이 기기(AMD Strix Halo iGPU)에서 출력이 0으로 붕괴해 사용 불가.
    //
    // intra_threads: 세션 하나가 op 병렬에 쓸 스레드 수. 세션 풀로 페이지를 병렬
    // 처리할 때, (워커 수 × intra_threads)가 코어를 과다구독하지 않도록 상위에서 제한.
    let mut builder = Session::builder().map_err(|e| e.to_string())?;
    if intra_threads > 0 {
        builder = builder
            .with_intra_threads(intra_threads)
            .map_err(|e| e.to_string())?;
    }
    builder.commit_from_file(path).map_err(|e| e.to_string())
}

/// cunet offset(출력에서 잘리는 테두리, 출력 px 기준). 입력 패딩 = offset/scale.
const OFFSET_1X: u32 = 28;
const OFFSET_2X: u32 = 36;

struct Pass {
    session: Session,
    pad: u32, // 추론 전 입력 사방에 넣을 엣지 패딩(입력 px)
}

pub struct Engine {
    passes: Vec<Pass>,
}

/// 옵션에서 실제 처리가 필요한지(엔진 로드 대상인지) 판정. 세션 풀 구성 전에
/// 워커 수/스레드 배분을 정하려면 로드 없이 먼저 알아야 해서 분리.
pub fn is_needed(opts: &ProcessOptions) -> bool {
    if !opts.use_waifu2x {
        return false;
    }
    let two_x = matches!(opts.upscale.as_str(), "2x" | "4x");
    let denoise = denoise_opt(&opts.denoise_level);
    base_model(two_x, denoise).is_some()
}

impl Engine {
    /// 옵션에 해당하는 엔진 구성. 처리할 게 없으면(1x·노이즈 없음) None.
    /// `intra_threads`: 세션 하나의 op 병렬 스레드 수(0이면 ort 기본).
    /// `quiet`: 세션 풀에서 여러 개 만들 때 로그 중복을 막기 위해 로그 억제.
    pub fn new(
        app: &AppHandle,
        opts: &ProcessOptions,
        intra_threads: usize,
        quiet: bool,
    ) -> Result<Option<Engine>, String> {
        if !opts.use_waifu2x {
            return Ok(None);
        }
        let factor = match opts.upscale.as_str() {
            "2x" => 2u32,
            "4x" => 4,
            _ => 1,
        };
        let two_x = factor >= 2;
        let denoise = denoise_opt(&opts.denoise_level);

        let first = match base_model(two_x, denoise) {
            Some(f) => f,
            None => return Ok(None),
        };

        // 1x 모델 offset 28(scale 1 → pad 28), 2x 모델 offset 36(scale 2 → pad 18).
        let first_pad = if two_x { OFFSET_2X / 2 } else { OFFSET_1X };
        if !quiet {
            log(app, "info", format!("waifu2x 모델 로드(CPU): {first}"));
        }
        let mut passes = vec![Pass {
            session: load_session(&model_path(app, &first), intra_threads)?,
            pad: first_pad,
        }];
        if factor == 4 {
            if !quiet {
                log(app, "info", "waifu2x 모델 로드(CPU): scale2x.onnx (4x 2번째 패스)".to_string());
            }
            passes.push(Pass {
                session: load_session(&model_path(app, "scale2x.onnx"), intra_threads)?,
                pad: OFFSET_2X / 2,
            });
        }
        Ok(Some(Engine { passes }))
    }

    pub fn process(&mut self, img: &RgbImage) -> Result<RgbImage, String> {
        let mut cur = img.clone();
        for i in 0..self.passes.len() {
            let pad = self.passes[i].pad;
            cur = run_pass(&mut self.passes[i].session, pad, &cur)?;
        }
        Ok(cur)
    }
}

/// 원본 `img`에서 (ox,oy)를 좌상단으로 하는 pw×ph 입력 텐서를 만들어 추론한다.
/// 각 입력 px는 `clamp(ox - pad + x, 0, w-1)`에서 샘플 → 전 이미지 경계에 엣지 복제.
/// 반환: (출력 f32 planar, ow, oh). 모델이 offset을 잘라내므로 ow ≈ (pw-2*pad)*scale.
fn infer(
    sess: &mut Session,
    img: &RgbImage,
    ox: i64,
    oy: i64,
    pad: u32,
    pw: u32,
    ph: u32,
) -> Result<(Vec<f32>, u32, u32), String> {
    let (w, h) = (img.width(), img.height());
    let plane = (pw * ph) as usize;
    let mut data = vec![0f32; 3 * plane];
    for y in 0..ph {
        let sy = ((oy - pad as i64 + y as i64).clamp(0, (h - 1) as i64)) as u32;
        for x in 0..pw {
            let sx = ((ox - pad as i64 + x as i64).clamp(0, (w - 1) as i64)) as u32;
            let px = img.get_pixel(sx, sy);
            let idx = (y * pw + x) as usize;
            data[idx] = px[0] as f32 / 255.0;
            data[plane + idx] = px[1] as f32 / 255.0;
            data[2 * plane + idx] = px[2] as f32 / 255.0;
        }
    }

    let input = ort::value::Tensor::from_array(([1usize, 3, ph as usize, pw as usize], data))
        .map_err(|e| e.to_string())?;
    let outputs = sess.run(ort::inputs![input]).map_err(|e| e.to_string())?;
    let (shape, out) = outputs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| e.to_string())?;
    let oh = shape[2] as u32;
    let ow = shape[3] as u32;
    Ok((out.to_vec(), ow, oh))
}

/// planar f32 출력의 (sx,sy)~(sx+cw,sy+ch) 영역을 dst의 (dx,dy)에 복사.
fn blit(dst: &mut RgbImage, out: &[f32], ow: u32, oh: u32, sx: u32, sy: u32, cw: u32, ch: u32, dx: u32, dy: u32) {
    let oplane = (ow * oh) as usize;
    for y in 0..ch {
        for x in 0..cw {
            let idx = ((sy + y) * ow + (sx + x)) as usize;
            let r = (out[idx] * 255.0).round().clamp(0.0, 255.0) as u8;
            let g = (out[oplane + idx] * 255.0).round().clamp(0.0, 255.0) as u8;
            let b = (out[2 * oplane + idx] * 255.0).round().clamp(0.0, 255.0) as u8;
            dst.put_pixel(dx + x, dy + y, Rgb([r, g, b]));
        }
    }
}

/// whole-image 추론(CPU 경로). 이미지 전체를 한 번에 모델에 넣는다.
fn run_pass(sess: &mut Session, pad: u32, img: &RgbImage) -> Result<RgbImage, String> {
    let (w, h) = (img.width(), img.height());

    // 사방 pad(입력 px)만큼 엣지 복제 + 4의 배수로 정렬(우/하단 여분).
    // 이렇게 하면 모델이 offset을 잘라도 출력 좌상단이 원본 (0,0)과 정렬됨.
    let pw = (w + 2 * pad + 3) / 4 * 4;
    let ph = (h + 2 * pad + 3) / 4 * 4;
    let (out, ow, oh) = infer(sess, img, 0, 0, pad, pw, ph)?;

    let scale = (ow as f32 / pw as f32).round().max(1.0) as u32;

    // 패딩 제거: 원본*scale 만큼만 좌상단 기준으로 크롭.
    let cw = (w * scale).min(ow);
    let ch = (h * scale).min(oh);
    let mut cropped = RgbImage::new(cw, ch);
    blit(&mut cropped, &out, ow, oh, 0, 0, cw, ch, 0, 0);
    Ok(cropped)
}
