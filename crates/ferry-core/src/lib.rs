//! Логика ferry: узлы и проект на них (хаб), демон рабочего стола и задачи (узел), релей
//! git, экран. Бинарник `ferry` (CLI) и приложение хаба — тонкие оболочки над ней.

pub mod desk;
pub mod doctor;
pub mod find;
pub mod hub;
pub mod jobs;
pub mod mode;
pub mod project;
pub mod relay;
pub mod screen;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

pub fn home() -> io::Result<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(var).ok_or_else(|| io::Error::other(format!("не задан {var}")))?;
    Ok(home.into())
}

/// `~/.ferry`: на узле — демон и его бинарник, на хабе — узлы, снимки экрана, патчи.
pub fn ferry_home() -> io::Result<PathBuf> {
    Ok(home()?.join(".ferry"))
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
