mod mail;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            mail::get_account,
            mail::save_account,
            mail::test_account,
            mail::list_folders,
            mail::list_messages,
            mail::get_message,
            mail::list_labels,
            mail::set_flag,
            mail::set_label,
            mail::move_message,
            mail::send_message,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
