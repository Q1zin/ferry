//! Режим устройства. Его выбирает человек за устройством — иконкой ferry в трее, в окне или
//! `ferry mode`; другие устройства режим только видят.
//!
//! - `active` — обычная работа;
//! - `no-desktop` — «не трогать рабочий стол»: команды `--desk`, ввод, снимки и задачи
//!   отклоняются, обычные команды (сборка, sync) идут;
//! - `paused` — «отключиться от других устройств»: устройство ничего не выполняет для других.
//!
//! Запреты проверяет само устройство, получив команду (`ferry-net`), и отказывает с кодом
//! [`REFUSED`].

use std::fs;
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Active,
    NoDesktop,
    Paused,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Active, Mode::NoDesktop, Mode::Paused];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Active => "active",
            Mode::NoDesktop => "no-desktop",
            Mode::Paused => "paused",
        }
    }

    pub fn parse(name: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.name() == name)
    }

    /// Для людей: в трее, в `ferry devices`, в приложении.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Active => "работает",
            Mode::NoDesktop => "не трогать рабочий стол",
            Mode::Paused => "отключён от других устройств",
        }
    }

    /// Можно ли сейчас трогать рабочий стол.
    pub fn allows_desktop(self) -> bool {
        self == Mode::Active
    }
}

/// Код выхода команды, которую узел отклонил из-за режима (EX_TEMPFAIL).
pub const REFUSED: i32 = 75;

pub fn current() -> Mode {
    crate::ferry_home().map_or(Mode::Paused, |dir| current_in(&dir))
}

pub fn set(mode: Mode) -> io::Result<()> {
    set_in(&crate::ferry_home()?, mode)
}

/// Первый запуск — работаем; повреждённый или недоступный режим не разрешает управление.
pub fn current_in(dir: &Path) -> Mode {
    match fs::read_to_string(dir.join("mode")) {
        Ok(text) => Mode::parse(text.trim()).unwrap_or(Mode::Paused),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Mode::Active,
        Err(_) => Mode::Paused,
    }
}

pub fn set_in(dir: &Path, mode: Mode) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    crate::write_private(&dir.join("mode"), &format!("{}\n", mode.name()))
}

/// `ferry mode [<режим>]` на узле.
pub fn command(new: Option<&str>) -> io::Result<i32> {
    if let Some(name) = new {
        let mode = Mode::parse(name).ok_or_else(|| {
            io::Error::other(format!(
                "режимы: active, no-desktop, paused — а не {name:?}"
            ))
        })?;
        set(mode)?;
    }
    let mode = current();
    println!("{} ({})", mode.name(), mode.label());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for mode in Mode::ALL {
            assert_eq!(Mode::parse(mode.name()), Some(mode));
        }
        assert_eq!(Mode::parse("paused\n"), None);
    }

    #[test]
    fn broken_mode_never_enables_remote_control() {
        let dir = std::env::temp_dir().join(format!("ferry-mode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(current_in(&dir), Mode::Active);
        for mode in Mode::ALL {
            set_in(&dir, mode).unwrap();
            assert_eq!(current_in(&dir), mode);
        }
        for broken in ["", "broken", "act", "paused\nactive"] {
            fs::write(dir.join("mode"), broken).unwrap();
            assert_eq!(current_in(&dir), Mode::Paused);
        }
        fs::remove_file(dir.join("mode")).unwrap();
        fs::create_dir(dir.join("mode")).unwrap();
        assert_eq!(current_in(&dir), Mode::Paused);
        fs::remove_dir_all(dir).unwrap();
    }
}
