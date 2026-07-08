mod events;
mod ncnn;
mod pdf;
mod pipeline;
mod waifu2x;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(pipeline::CancelFlag::default())
        .manage(pipeline::PreparedPages::default())
        .invoke_handler(tauri::generate_handler![
            pipeline::prepare_pages,
            pipeline::start_processing,
            pipeline::cancel_processing
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
