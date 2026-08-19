// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if std::env::args().any(|argument| argument == "--import-password-from-stdin") {
        if let Err(error) = yandex_mail_lib::import_password_from_stdin() {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    yandex_mail_lib::run()
}
