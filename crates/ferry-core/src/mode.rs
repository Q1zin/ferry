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
    crate::ferry_home().map_or(Mode::Active, |dir| current_in(&dir))
}

pub fn set(mode: Mode) -> io::Result<()> {
    set_in(&crate::ferry_home()?, mode)
}

/// Режим из `<dir>/mode`; нет файла — работаем.
pub fn current_in(dir: &Path) -> Mode {
    fs::read_to_string(dir.join("mode"))
        .ok()
        .and_then(|text| Mode::parse(text.trim()))
        .unwrap_or(Mode::Active)
}

pub fn set_in(dir: &Path, mode: Mode) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("mode"), format!("{}\n", mode.name()))
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
}
