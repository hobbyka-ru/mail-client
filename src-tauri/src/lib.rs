mod cache;
mod mail;

pub fn import_password_from_stdin() -> Result<(), String> {
    mail::import_password_from_stdin()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(mail::WatchState::default())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            mail::get_account,
            mail::replace_account,
            mail::test_account,
            mail::get_cached_folders,
            mail::create_folder,
            mail::rename_folder,
            mail::delete_folder,
            mail::get_cached_messages,
            mail::search_cached_messages,
            mail::get_cached_labels,
            mail::create_label,
            mail::rename_label,
            mail::delete_label,
            mail::list_folders,
            mail::list_messages,
            mail::get_message,
            mail::list_labels,
            mail::set_flag,
            mail::set_label,
            mail::move_message,
            mail::apply_message_action,
            mail::save_attachment,
            mail::open_attachment,
            mail::prepare_image_previews,
            mail::save_all_attachments,
            mail::inspect_attachments,
            mail::prepare_forward_attachments,
            mail::watch_folder,
            mail::stop_watching,
            mail::send_message,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
