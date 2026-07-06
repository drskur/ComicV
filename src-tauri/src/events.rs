//! 프론트엔드로 보내는 진행 이벤트(로그/진행률/완료)의 단일 창구.

use serde::Serialize;
use tauri::{AppHandle, Emitter};

pub const LOG: &str = "process://log";
pub const PROGRESS: &str = "process://progress";
pub const DONE: &str = "process://done";

#[derive(Clone, Serialize)]
struct LogEvent {
    level: String,
    message: String,
}

#[derive(Clone, Serialize)]
struct ProgressEvent {
    current: usize,
    total: usize,
    percent: u32,
}

/// level: "info" | "success" | "warn" | "error"
pub fn log(app: &AppHandle, level: &str, message: impl Into<String>) {
    let _ = app.emit(
        LOG,
        LogEvent {
            level: level.to_string(),
            message: message.into(),
        },
    );
}

pub fn progress(app: &AppHandle, current: usize, total: usize) {
    let percent = if total == 0 {
        0
    } else {
        (current * 100 / total) as u32
    };
    let _ = app.emit(
        PROGRESS,
        ProgressEvent {
            current,
            total,
            percent,
        },
    );
}

pub fn done(app: &AppHandle) {
    let _ = app.emit(DONE, ());
}
