//! Команды на рабочем столе устройства: его шелл и остановка дерева процессов. Приложение
//! ferry живёт в сессии пользователя, поэтому всё, что оно запускает, видит рабочий стол.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};

/// Предел поля в протоколе клиента релея: мусор на порту не должен заставить выделить гигабайты.
const MAX_FIELD: u32 = 1 << 20;

#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Поле «`u32` LE длина + байты».
pub fn read_field(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FIELD {
        return Err(io::Error::other("слишком длинное поле"));
    }
    let mut buf = vec![0; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn write_field(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    w.write_all(&(data.len() as u32).to_le_bytes())?;
    w.write_all(data)
}

/// Шелл устройства для команды: Git Bash на Windows, login-шелл пользователя на unix.
pub fn shell(cmd: &str) -> Command {
    let mut command = Command::new(shell_path());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Без login: login-шелл Git Bash на `exit` печатает clear из /etc/bash.bash_logout. Окно
        // консоли команде не нужно; GUI-программы, которые она запустит, это не затрагивает.
        command.args(["-c", cmd]).creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Login-шелл: из него PATH с brew и прочим из профиля. Своя группа процессов — чтобы
        // при обрыве убить всё дерево разом.
        command.args(["-lc", cmd]).process_group(0);
    }
    command
}

fn shell_path() -> OsString {
    if let Some(shell) = std::env::var_os("FERRY_SHELL") {
        return shell;
    }
    if cfg!(windows) {
        r"C:\Program Files\Git\bin\bash.exe".into()
    } else {
        std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into())
    }
}

/// Остановить команду вместе со всем, что она запустила.
// ponytail: taskkill /t идёт по ссылкам на родителя, поэтому процессы, чей родитель уже
// вышел, переживут обрыв; если начнут мешать — Job Object с KILL_ON_JOB_CLOSE.
pub fn kill_tree(pid: u32) -> io::Result<()> {
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("taskkill");
        command
            .args(["/pid", &pid.to_string(), "/t", "/f"])
            .creation_flags(CREATE_NO_WINDOW);
        command
    };
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("kill");
        command.args(["-TERM", "--", &format!("-{pid}")]);
        command
    };
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(status.to_string()))
    }
}
