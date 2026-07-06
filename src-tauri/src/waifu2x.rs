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
    #[allow(unused_mut)]
    let mut builder = Session::builder().map_err(|e| e.to_string())?;

    // 기본 CPU(정확). DirectML은 입력 크기에 따라 출력이 붕괴하는 오산이 있어
    // 아직 신뢰 불가 → 실험적 opt-in(COMICV_DML=1). 안정화는 고정 타일로 예정.
    let use_dml = std::env::var("COMICV_DML").ok().as_deref() == Some("1");
    #[cfg(windows)]
    if use_dml {
        // DML이 융합된 연산을 오산하는 걸 우회: 그래프 최적화 끄기.
        builder = builder
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Disable)
            .map_err(|e| e.to_string())?
            .with_execution_providers([ort::ep::directml::DirectML::default().build()])
            .map_err(|e| e.to_string())?;
    }
    emit_log(
        app,
        "info",
        format!("EP: {}", if use_dml { "DirectML" } else { "CPU" }),
    );

    builder.commit_from_file(&path).map_err(|e| e.to_string())
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

        // 1x 모델 offset 28(scale 1 → pad 28), 2x 모델 offset 36(scale 2 → pad 18).
        let first_pad = if two_x { OFFSET_2X / 2 } else { OFFSET_1X };
        emit_log(app, "info", format!("waifu2x 모델 로드: {first}"));
        let mut passes = vec![Pass {
            session: load_session(app, &first)?,
            pad: first_pad,
        }];
        if factor == 4 {
            emit_log(app, "info", "waifu2x 모델 로드: scale2x.onnx (4x 2번째 패스)".to_string());
            passes.push(Pass {
                session: load_session(app, "scale2x.onnx")?,
                pad: OFFSET_2X / 2,
            });
        }
        Ok(Some(Engine { passes }))
    }

    pub fn process(&mut self, img: &RgbImage, app: &AppHandle) -> Result<RgbImage, String> {
        let total = self.passes.len();
        let mut cur = img.clone();
        for i in 0..self.passes.len() {
            let pad = self.passes[i].pad;
            cur = run_pass(&mut self.passes[i].session, pad, &cur, app, i + 1, total)?;
        }
        Ok(cur)
    }
}

fn run_pass(
    sess: &mut Session,
    pad: u32,
    img: &RgbImage,
    app: &AppHandle,
    pass: usize,
    total: usize,
) -> Result<RgbImage, String> {
    let (w, h) = (img.width(), img.height());

    // 사방 pad(입력 px)만큼 엣지 복제 + 4의 배수로 정렬(우/하단 여분).
    // 이렇게 하면 모델이 offset을 잘라도 출력 좌상단이 원본 (0,0)과 정렬됨.
    let pw = (w + 2 * pad + 3) / 4 * 4;
    let ph = (h + 2 * pad + 3) / 4 * 4;
    let plane = (pw * ph) as usize;
    let mut data = vec![0f32; 3 * plane];
    for y in 0..ph {
        let sy = ((y as i64 - pad as i64).clamp(0, (h - 1) as i64)) as u32;
        for x in 0..pw {
            let sx = ((x as i64 - pad as i64).clamp(0, (w - 1) as i64)) as u32;
            let px = img.get_pixel(sx, sy);
            let idx = (y * pw + x) as usize;
            data[idx] = px[0] as f32 / 255.0;
            data[plane + idx] = px[1] as f32 / 255.0;
            data[2 * plane + idx] = px[2] as f32 / 255.0;
        }
    }

    let input =
        ort::value::Tensor::from_array(([1usize, 3, ph as usize, pw as usize], data))
            .map_err(|e| e.to_string())?;
    let outputs = sess.run(ort::inputs![input]).map_err(|e| e.to_string())?;
    let (shape, out) = outputs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| e.to_string())?;
    let oh = shape[2] as u32;
    let ow = shape[3] as u32;

    // 출력 붕괴 감지(예: DML 오산). 정상이면 조용히 넘어감.
    if pass == 1 {
        let out_mean = out.iter().map(|&v| v as f64).sum::<f64>() / out.len() as f64;
        if out_mean < 0.05 {
            emit_log(
                app,
                "warn",
                format!("출력이 비정상적으로 어두움(mean={out_mean:.4}) — 엔진/EP 확인 필요"),
            );
        }
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
