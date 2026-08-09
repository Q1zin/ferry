//! Режим узла. Его выбирает человек за узлом — иконкой ferry в трее или `ferry mode`; хаб
//! режим только видит.
//!
//! - `active` — обычная работа;
//! - `no-desktop` — «не трогать рабочий стол»: команды `--desk`, ввод, снимки и задачи
//!   отклоняются, обычные команды (сборка, sync) идут;
//! - `paused` — «отключиться от хаба»: хаб ничего не делает на узле.
//!
//! Рабочий стол бережёт сам демон — это жёсткий запрет. Остальное на паузе отклоняет
//! проверка, с которой хаб начинает каждую команду на узле ([`GUARD`]).

use std::fs;
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    /// Для людей: в трее, в `ferry ls`, в приложении хаба.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Active => "работает",
            Mode::NoDesktop => "не трогать рабочий стол",
            Mode::Paused => "отключён от хаба",
        }
    }

    /// Можно ли сейчас трогать рабочий стол.
    pub fn allows_desktop(self) -> bool {
        self == Mode::Active
    }
}

/// Код выхода команды, которую узел отклонил из-за режима (EX_TEMPFAIL).
pub const REFUSED: i32 = 75;

/// Начало каждой команды хаба на узле: на паузе — выйти с [`REFUSED`] и объяснением.
pub const GUARD: &str = "[ \"$(cat ~/.ferry/mode 2>/dev/null)\" = paused ] && \
    { echo 'ferry: узел отключён от хаба — это решили на самом узле (иконка ferry в трее)' >&2; \
    exit 75; }; ";

pub fn current() -> Mode {
    let Ok(path) = path() else {
        return Mode::Active;
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|text| Mode::parse(text.trim()))
        .unwrap_or(Mode::Active)
}

pub fn set(mode: Mode) -> io::Result<()> {
    let path = path()?;
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, format!("{}\n", mode.name()))
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

fn path() -> io::Result<std::path::PathBuf> {
    Ok(crate::ferry_home()?.join("mode"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn names_round_trip() {
        for mode in Mode::ALL {
            assert_eq!(Mode::parse(mode.name()), Some(mode));
        }
        assert_eq!(Mode::parse("paused\n"), None);
    }

    /// GUARD в настоящем sh: на паузе — код 75 и команда не выполняется, иначе — выполняется.
    #[test]
    fn guard_stops_commands_only_when_paused() {
        let home = std::env::temp_dir().join(format!("ferry-guard-{}", std::process::id()));
        fs::create_dir_all(home.join(".ferry")).unwrap();
        let run = || {
            Command::new("sh")
                .args(["-c", &format!("{GUARD}echo ran")])
                .env("HOME", &home)
                .output()
                .unwrap()
        };
        for (mode, code, out) in [
            ("paused", 75, ""),
            ("no-desktop", 0, "ran\n"),
            ("active", 0, "ran\n"),
        ] {
            fs::write(home.join(".ferry/mode"), format!("{mode}\n")).unwrap();
            let result = run();
            assert_eq!(result.status.code(), Some(code), "{mode}");
            assert_eq!(String::from_utf8_lossy(&result.stdout), out, "{mode}");
        }
        fs::remove_file(home.join(".ferry/mode")).unwrap();
        assert_eq!(run().status.code(), Some(0), "без файла режима — работаем");
        let _ = fs::remove_dir_all(&home);
    }
}
