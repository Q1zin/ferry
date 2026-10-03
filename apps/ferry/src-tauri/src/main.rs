// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Релей git ядро поднимает своим же бинарником с аргументом `relay` — в приложении это
    // оно само, и вместо второго окна должен запуститься релей.
    if std::env::args().nth(1).as_deref() == Some("relay") {
        let code = ferry_core::relay::serve().unwrap_or_else(|e| {
            eprintln!("ferry relay: {e}");
            255
        });
        std::process::exit(code);
    }
    ferry_app_lib::run()
}
