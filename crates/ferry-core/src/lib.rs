//! Логика ferry на устройстве: шелл и задачи, экран и ввод, свойства, снимок проекта, режим,
//! релей git; общие типы устройств и локальное управление приложением. CLI и приложение —
//! тонкие оболочки над ней, связь устройств — в `ferry-net`.

pub mod control;
pub mod crashes;
pub mod desk;
pub mod device;
pub mod doctor;
pub mod info;
pub mod jobs;
pub mod junit;
pub mod mode;
pub mod ocr;
pub mod power;
pub mod project;
pub mod relay;
pub mod screen;
pub mod sync;
pub mod update;

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

/// Атомарно заменить файл, доступный только владельцу (на Windows профиль и так закрыт).
pub fn write_private(path: &Path, text: &str) -> io::Result<()> {
    write_private_bytes(path, text.as_bytes())
}

pub(crate) fn write_private_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("нужно имя файла"))?;
    let (temp, mut file) = loop {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let temp = path.with_file_name(format!(
            ".{}.{}.{id}.tmp",
            name.to_string_lossy(),
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_writes_replace_whole_files_and_clean_up_on_failure() {
        let dir = std::env::temp_dir().join(format!("ferry-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state");
        write_private(&path, "before").unwrap();
        std::thread::scope(|threads| {
            let path = &path;
            threads.spawn(move || {
                for _ in 0..1000 {
                    let text = std::fs::read_to_string(path).unwrap();
                    assert!(
                        matches!(text.as_str(), "before" | "after"),
                        "partial state: {text:?}"
                    );
                }
            });
            for _ in 0..20 {
                write_private(path, "after").unwrap();
                write_private(path, "before").unwrap();
            }
        });
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let folder = dir.join("folder");
        std::fs::create_dir_all(&folder).unwrap();
        assert!(write_private(&folder, "cannot replace a directory").is_err());
        assert!(folder.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
