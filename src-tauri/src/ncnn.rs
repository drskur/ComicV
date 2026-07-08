//! GPU 가속 waifu2x — `waifu2x-ncnn-vulkan`(Vulkan) 바이너리 배치 호출.
//!
//! DirectML(ort)은 대상 기기(AMD Strix Halo iGPU)에서 출력이 0으로 붕괴해 사용 불가라,
//! GPU 경로는 Vulkan 백엔드인 ncnn 바이너리를 번들해 프로세스로 호출한다.
//!
//! 번들 위치(리소스):
//!   resources/waifu2x-ncnn-vulkan/
//!     waifu2x-ncnn-vulkan(.exe)
//!     models-cunet/        ← cunet용 .param/.bin (noise{n}[_scale2.0x]_model.*)
//!
//! 배치 모드: 입력 폴더를 통째로 넘겨(-i in -o out) 프로세스를 한 번만 띄운다.
//! 진행률은 출력 폴더에 파일이 생기는 개수를 폴링해서 추정한다.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::events::log;
use crate::pipeline::ProcessOptions;

const EXE_NAME: &str = if cfg!(windows) {
    "waifu2x-ncnn-vulkan.exe"
} else {
    "waifu2x-ncnn-vulkan"
};

/// Windows 확장 경로 접두사 `\\?\`를 제거한다.
/// Tauri의 resource resolve는 verbatim(`\\?\C:\...`) 경로를 돌려주는데, ncnn은
/// 모델 파일명을 `/`로 이어붙여 열기 때문에 verbatim 경로에선 `_wfopen`이 실패한다.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

/// 번들된 실행 파일 경로. 리소스 우선, 없으면 개발 모드 폴백.
fn exe_path(app: &AppHandle) -> PathBuf {
    if let Ok(p) = app.path().resolve(
        format!("resources/waifu2x-ncnn-vulkan/{EXE_NAME}"),
        BaseDirectory::Resource,
    ) {
        if p.exists() {
            return strip_verbatim(p);
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("waifu2x-ncnn-vulkan")
        .join(EXE_NAME)
}

/// cunet 모델 폴더(.param/.bin 모음). 실행 파일이 -n/-s에 맞는 파일을 알아서 고른다.
fn models_dir(app: &AppHandle) -> PathBuf {
    if let Ok(p) = app.path().resolve(
        "resources/waifu2x-ncnn-vulkan/models-cunet",
        BaseDirectory::Resource,
    ) {
        if p.exists() {
            return strip_verbatim(p);
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("waifu2x-ncnn-vulkan")
        .join("models-cunet")
}

/// 번들 바이너리가 존재하는가(GPU 경로 사용 가능 여부).
pub fn is_available(app: &AppHandle) -> bool {
    exe_path(app).exists()
}

/// 옵션 → (scale, noise). ncnn 인자는 scale 1/2/4, noise -1/0..3.
/// 스케일도 노이즈도 없으면(1x·denoise none) 처리할 게 없어 None.
pub fn args_for(opts: &ProcessOptions) -> Option<(u32, i32)> {
    let scale = match opts.upscale.as_str() {
        "2x" => 2,
        "4x" => 4,
        _ => 1,
    };
    let noise = match opts.denoise_level.as_str() {
        "0" => 0,
        "1" => 1,
        "2" => 2,
        "3" => 3,
        _ => -1,
    };
    if scale == 1 && noise < 0 {
        None
    } else {
        Some((scale, noise))
    }
}

/// 입력 폴더 전체를 배치 처리해 출력 폴더에 PNG로 저장한다.
/// - `total`: 진행률 계산용 전체 페이지 수
/// - `done_base`: 이 배치 이전에 이미 완료로 친 페이지 수(진행률 오프셋)
/// - 취소 시 자식 프로세스를 kill 하고 조기 반환.
pub fn run_batch(
    app: &AppHandle,
    in_dir: &Path,
    out_dir: &Path,
    scale: u32,
    noise: i32,
    cancel: &AtomicBool,
    on_progress: impl Fn(usize),
) -> Result<(), String> {
    let exe = exe_path(app);
    if !exe.exists() {
        return Err(format!("ncnn 실행 파일 없음: {}", exe.display()));
    }
    let models = models_dir(app);
    if !models.exists() {
        return Err(format!("ncnn 모델 폴더 없음: {}", models.display()));
    }

    log(
        app,
        "info",
        format!("GPU(ncnn/Vulkan) 배치: scale={scale} noise={noise}"),
    );

    let mut cmd = Command::new(&exe);
    cmd.arg("-i")
        .arg(in_dir)
        .arg("-o")
        .arg(out_dir)
        .arg("-m")
        .arg(&models)
        .arg("-s")
        .arg(scale.to_string())
        .arg("-n")
        .arg(noise.to_string())
        .arg("-f")
        .arg("png") // 무손실 중간물 — 최종 품질/포맷은 파이프라인이 다시 인코딩
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // Windows에서 콘솔 창이 뜨지 않게.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("ncnn 실행 실패({}): {e}", exe.display()))?;

    // stderr를 별도 스레드로 흡수(진행 폴링과 병행). 실패 시 진단에 사용.
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    if let Some(mut err) = child.stderr.take() {
        let buf = Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            if let Ok(mut guard) = buf.lock() {
                *guard = s;
            }
        });
    }

    // 자식이 끝날 때까지 출력 폴더 파일 수를 폴링해 진행률 산출.
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(()); // 취소는 오류가 아님 — 상위에서 부분 저장 처리
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    on_progress(count_pngs(out_dir));
                    return Ok(());
                }
                let tail = stderr_buf
                    .lock()
                    .ok()
                    .map(|g| {
                        // 마지막 400자만(진단용 꼬리).
                        let n = g.chars().count().saturating_sub(400);
                        g.chars().skip(n).collect::<String>()
                    })
                    .unwrap_or_default();
                return Err(format!("ncnn 비정상 종료({status}). {tail}"));
            }
            Ok(None) => {
                on_progress(count_pngs(out_dir));
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => return Err(format!("ncnn 프로세스 감시 실패: {e}")),
        }
    }
}

fn count_pngs(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .extension()
                        .and_then(|x| x.to_str())
                        .map(|x| x.eq_ignore_ascii_case("png"))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}
