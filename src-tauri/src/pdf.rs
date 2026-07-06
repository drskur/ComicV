//! PDF → 이미지 리스트.
//!
//! 스캔 만화 PDF는 페이지당 이미지 1장이 박혀 있는 경우가 대부분이라,
//! 래스터화(렌더링)하지 않고 원본 이미지 XObject를 그대로 추출한다(화질 보존).
//! 추출한 이미지는 임시 폴더에 쓰고 그 경로 목록을 돌려준다.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use image::{GrayImage, RgbImage};
use lopdf::Document;
use tauri::{AppHandle, Emitter};

fn emit_log(app: &AppHandle, level: &str, msg: String) {
    let _ = app.emit(
        "process://log",
        serde_json::json!({ "level": level, "message": msg }),
    );
}

/// PDF에서 이미지를 추출해 임시 파일 경로 목록으로 반환. 실패는 경고 로그 후 스킵.
pub fn extract_pdf(path: &Path, app: &AppHandle) -> Vec<PathBuf> {
    let doc = match Document::load(path) {
        Ok(d) => d,
        Err(e) => {
            emit_log(app, "warn", format!("PDF 열기 실패 {}: {e}", path.display()));
            return Vec::new();
        }
    };

    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pdf".to_string());
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir()
        .join("comicv")
        .join(format!("{stem}_{nonce}"));
    if let Err(e) = fs::create_dir_all(&tmp) {
        emit_log(app, "warn", format!("임시 폴더 생성 실패: {e}"));
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut idx = 0usize;
    for (page_num, page_id) in doc.get_pages() {
        let images = match doc.get_page_images(page_id) {
            Ok(v) => v,
            Err(e) => {
                emit_log(app, "warn", format!("{page_num}페이지 이미지 추출 실패: {e}"));
                continue;
            }
        };
        for img in images {
            idx += 1;
            let filters = img.filters.clone().unwrap_or_default();
            let has = |name: &str| filters.iter().any(|f| f == name);

            // 1) DCTDecode → 스트림이 곧 JPEG. 그대로 저장(무손실 통과).
            if has("DCTDecode") {
                let p = tmp.join(format!("p{idx:04}.jpg"));
                match fs::write(&p, img.content) {
                    Ok(_) => out.push(p),
                    Err(e) => emit_log(app, "warn", format!("{page_num}페이지 저장 실패: {e}")),
                }
                continue;
            }
            if has("JPXDecode") {
                emit_log(
                    app,
                    "warn",
                    format!("{page_num}페이지 스킵: JPEG2000(JPXDecode) 미지원"),
                );
                continue;
            }

            // 2) 완결된 이미지 바이트로 디코드 시도(드문 경우).
            if let Ok(dynimg) = image::load_from_memory(img.content) {
                let p = tmp.join(format!("p{idx:04}.png"));
                match dynimg.to_rgb8().save(&p) {
                    Ok(_) => out.push(p),
                    Err(e) => emit_log(app, "warn", format!("{page_num}페이지 저장 실패: {e}")),
                }
                continue;
            }

            // 3) 원본 샘플(무압축)에서 재구성: DeviceRGB/DeviceGray 8bpc.
            let w = img.width.max(0) as u32;
            let h = img.height.max(0) as u32;
            let bpc = img.bits_per_component.unwrap_or(8);
            let cs = img.color_space.clone().unwrap_or_default().to_ascii_lowercase();
            let n = img.content.len();
            let saved = if bpc == 8 && w > 0 && h > 0 {
                let px = (w as usize) * (h as usize);
                if n == px * 3 && (cs.contains("rgb") || cs.is_empty()) {
                    RgbImage::from_raw(w, h, img.content.to_vec())
                        .and_then(|b| b.save(tmp.join(format!("p{idx:04}.png"))).ok())
                        .is_some()
                } else if n == px && cs.contains("gray") {
                    GrayImage::from_raw(w, h, img.content.to_vec())
                        .and_then(|b| b.save(tmp.join(format!("p{idx:04}.png"))).ok())
                        .is_some()
                } else {
                    false
                }
            } else {
                false
            };

            if saved {
                out.push(tmp.join(format!("p{idx:04}.png")));
            } else {
                emit_log(
                    app,
                    "warn",
                    format!(
                        "{page_num}페이지 스킵: 미지원 이미지(filters={filters:?}, cs={cs}, bpc={bpc}, {w}x{h}, {n}B)"
                    ),
                );
            }
        }
    }

    emit_log(
        app,
        "info",
        format!(
            "PDF에서 {}장 추출: {}",
            out.len(),
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
    );
    out
}
