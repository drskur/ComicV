//! waifu2x(cunet) ONNX 추론 — ort + DirectML.
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
use tauri::{path::BaseDirectory, AppHandle, Emitter, Manager};

use crate::pipeline::ProcessOptions;

fn emit_log(app: &AppHandle, level: &str, msg: String) {
    let _ = app.emit(
        "process://log",
        serde_json::json!({ "level": level, "message": msg }),
    );
}

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

fn load_session(app: &AppHandle, name: &str) -> Result<Session, String> {
    let path = model_path(app, name);
    if !path.exists() {
        return Err(format!("모델 파일 없음: {}", path.display()));
    }
    let mut builder = Session::builder().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        builder = builder
            .with_execution_providers([ort::ep::directml::DirectML::default().build()])
            .map_err(|e| e.to_string())?;
    }
    builder.commit_from_file(&path).map_err(|e| e.to_string())
}

pub struct Engine {
    passes: Vec<Session>,
}

impl Engine {
    /// 옵션에 해당하는 엔진 구성. 처리할 게 없으면(1x·노이즈 없음) None.
    pub fn new(app: &AppHandle, opts: &ProcessOptions) -> Result<Option<Engine>, String> {
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

        emit_log(app, "info", format!("waifu2x 모델 로드: {first}"));
        let mut passes = vec![load_session(app, &first)?];
        if factor == 4 {
            emit_log(app, "info", "waifu2x 모델 로드: scale2x.onnx (4x 2번째 패스)".to_string());
            passes.push(load_session(app, "scale2x.onnx")?);
        }
        Ok(Some(Engine { passes }))
    }

    pub fn process(&mut self, img: &RgbImage, app: &AppHandle) -> Result<RgbImage, String> {
        let total = self.passes.len();
        let mut cur = img.clone();
        for (i, sess) in self.passes.iter_mut().enumerate() {
            cur = run_pass(sess, &cur, app, i + 1, total)?;
        }
        Ok(cur)
    }
}

fn run_pass(
    sess: &mut Session,
    img: &RgbImage,
    app: &AppHandle,
    pass: usize,
    total: usize,
) -> Result<RgbImage, String> {
    let (w, h) = (img.width(), img.height());

    // cunet은 입력 크기가 4의 배수여야 함 → 우/하단 엣지 복제 패딩.
    let pw = (w + 3) / 4 * 4;
    let ph = (h + 3) / 4 * 4;
    let plane = (pw * ph) as usize;
    let mut data = vec![0f32; 3 * plane];
    for y in 0..ph {
        let sy = y.min(h - 1);
        for x in 0..pw {
            let sx = x.min(w - 1);
            let px = img.get_pixel(sx, sy);
            let idx = (y * pw + x) as usize;
            data[idx] = px[0] as f32 / 255.0;
            data[plane + idx] = px[1] as f32 / 255.0;
            data[2 * plane + idx] = px[2] as f32 / 255.0;
        }
    }

    // 진단: 입력 값 범위
    let in_mean = data.iter().map(|&v| v as f64).sum::<f64>() / data.len() as f64;

    let input =
        ort::value::Tensor::from_array(([1usize, 3, ph as usize, pw as usize], data))
            .map_err(|e| e.to_string())?;
    let outputs = sess.run(ort::inputs![input]).map_err(|e| e.to_string())?;
    let (shape, out) = outputs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| e.to_string())?;
    let oh = shape[2] as u32;
    let ow = shape[3] as u32;

    // 진단: 출력 값 범위 (검은 결과물 원인 추적)
    if pass == 1 {
        let mut mn = f32::MAX;
        let mut mx = f32::MIN;
        let mut sum = 0f64;
        for &v in out.iter() {
            if v < mn {
                mn = v;
            }
            if v > mx {
                mx = v;
            }
            sum += v as f64;
        }
        let out_mean = sum / out.len() as f64;
        emit_log(
            app,
            "info",
            format!(
                "진단 shape=[{:?}] 입력mean={in_mean:.4} 출력 min={mn:.4} max={mx:.4} mean={out_mean:.4}",
                &shape
            ),
        );
    }
    let scale = (ow as f32 / pw as f32).round().max(1.0) as u32;

    let oplane = (ow * oh) as usize;
    let mut full = RgbImage::new(ow, oh);
    for y in 0..oh {
        for x in 0..ow {
            let idx = (y * ow + x) as usize;
            let r = (out[idx] * 255.0).round().clamp(0.0, 255.0) as u8;
            let g = (out[oplane + idx] * 255.0).round().clamp(0.0, 255.0) as u8;
            let b = (out[2 * oplane + idx] * 255.0).round().clamp(0.0, 255.0) as u8;
            full.put_pixel(x, y, Rgb([r, g, b]));
        }
    }

    // 패딩 제거: 원본*scale 만큼만 좌상단 기준으로 크롭.
    let cw = (w * scale).min(ow);
    let ch = (h * scale).min(oh);
    let cropped = image::imageops::crop_imm(&full, 0, 0, cw, ch).to_image();

    if pass == 1 {
        emit_log(
            app,
            "info",
            format!("waifu2x {pass}/{total}: {w}x{h} → {ow}x{oh} (scale {scale})"),
        );
    }
    Ok(cropped)
}
