//! Логика ferry на устройстве: шелл и задачи, экран и ввод, свойства, снимок проекта, режим,
//! релей git; общие типы устройств и локальное управление приложением. CLI и приложение —
//! тонкие оболочки над ней, связь устройств — в `ferry-net`.

pub mod control;
pub mod desk;
pub mod device;
pub mod doctor;
pub mod info;
pub mod jobs;
pub mod mode;
pub mod power;
pub mod project;
pub mod relay;
pub mod screen;
pub mod sync;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

/// Версия ferry — у хаба и узлов она должна совпадать.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn home() -> io::Result<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(var).ok_or_else(|| io::Error::other(format!("не задан {var}")))?;
    Ok(home.into())
}

/// `~/.ferry`: на узле — демон и его бинарник, на хабе — узлы, снимки экрана, патчи.
pub fn ferry_home() -> io::Result<PathBuf> {
    Ok(home()?.join(".ferry"))
}

/// Сейчас, секунды Unix.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Лог приложения ferry в `~/.ferry`: `ferry logs` читает его хвост, в том числе издалека.
pub fn app_log(dir: &Path) -> PathBuf {
    dir.join("logs").join("ferry.log")
}

/// Записать файл, доступный только владельцу (на Windows профиль и так закрыт).
pub fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(text.as_bytes())
}

/// Строка в одинарных кавычках для POSIX-шелла узла (там Git Bash или zsh).
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(255)
}
