// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Надсмотрщик фоновой задачи — этот же exe: задачи живут отдельно от окна и переживают
    // перезапуск приложения.
    let args: Vec<String> = std::env::args().collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = match args.as_slice() {
        [_, "job-supervise", name] => Some(ferry_core::jobs::supervise(name)),
        // git на этом устройстве ходит в приватные репозитории ключами устройства, которое
        // запустило команду: GIT_SSH_COMMAND='<этот exe> ssh' (релей git).
        [_, "ssh", rest @ ..] => Some(ferry_core::relay::ssh(rest)),
        _ => None,
    };
    if let Some(code) = code {
        std::process::exit(code.unwrap_or_else(|e| {
            eprintln!("ferry: {e}");
            255
        }));
    }
    ferry_app_lib::run()
}
