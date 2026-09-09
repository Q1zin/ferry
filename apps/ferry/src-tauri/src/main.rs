// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Надсмотрщик фоновой задачи — этот же exe: задачи живут отдельно от окна и переживают
    // перезапуск приложения.
    if let [_, cmd, name] = std::env::args().collect::<Vec<_>>().as_slice()
        && cmd == "job-supervise"
    {
        let code = ferry_core::jobs::supervise(name).unwrap_or_else(|e| {
            eprintln!("ferry: {e}");
            255
        });
        std::process::exit(code);
    }
    ferry_app_lib::run()
}
