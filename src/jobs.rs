//! Фоновые задачи на узле: команда на рабочем столе, которая живёт дольше вызова, — запущенное
//! приложение, dev-сервер, долгая сборка.
//!
//! `ferry job <имя> <команда>` (через `ferry desk`, то есть на рабочем столе) поднимает
//! отдельный процесс-надсмотрщик `ferry job-supervise <имя>` и сразу выходит. Надсмотрщик
//! запускает команду с выводом в лог, ждёт её и записывает код выхода. Поэтому задача
//! переживает и обрыв ssh, и перезапуск демона (`ferry update`, `ferry restart`).
//!
//! Всё состояние — файлы в `~/.ferry/jobs/<имя>/`: `cmd`, `cwd`, `started`, `pid`
//! (надсмотрщик), `child` (команда), `log`, `exit`, `killed`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::desk::{kill_tree, shell};
use crate::exit_code;

/// Запустить задачу; задача с тем же именем, если ещё работает, перезапускается.
pub fn start(name: &str, cmd: &str) -> io::Result<i32> {
    let alias = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    if name.is_empty() || name.starts_with('-') || !name.chars().all(alias) {
        return Err(io::Error::other(format!(
            "недопустимое имя задачи: {name:?}"
        )));
    }
    let dir = dir(name)?;
    if running(&dir) {
        kill(name)?;
    }
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("cmd"), cmd)?;
    fs::write(
        dir.join("cwd"),
        std::env::current_dir()?.to_string_lossy().as_bytes(),
    )?;
    fs::write(dir.join("started"), now().to_string())?;
    fs::File::create(dir.join("log"))?;

    let mut supervisor = Command::new(std::env::current_exe()?);
    supervisor
        .args(["job-supervise", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut supervisor);
    let pid = supervisor.spawn()?.id();
    fs::write(dir.join("pid"), pid.to_string())?;
    println!("ferry: задача {name} запущена");
    Ok(0)
}

/// `ferry job-supervise <имя>`: запустить команду задачи, дождаться, записать код выхода.
pub fn supervise(name: &str) -> io::Result<i32> {
    let dir = dir(name)?;
    let cmd = fs::read_to_string(dir.join("cmd"))?;
    let cwd = fs::read_to_string(dir.join("cwd"))?;
    // Лог создан пустым при запуске, пишет только надсмотрщик — хватит обычной записи.
    // Дозапись не годится: на Windows такой дескриптор без FILE_WRITE_DATA, и MSYS-bash
    // в него молча не пишет.
    let log = fs::OpenOptions::new().write(true).open(dir.join("log"))?;
    let mut command = shell(&cmd);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    let code = match command.spawn() {
        Ok(mut child) => {
            fs::write(dir.join("child"), child.id().to_string())?;
            exit_code(child.wait()?)
        }
        Err(e) => {
            writeln!(&log, "ferry: не удалось запустить: {e}")?;
            127
        }
    };
    fs::write(dir.join("exit"), code.to_string())?;
    Ok(code)
}

pub fn list() -> io::Result<i32> {
    let mut names: Vec<String> = match fs::read_dir(crate::ferry_home()?.join("jobs")) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    if names.is_empty() {
        eprintln!("ferry: задач нет");
        return Ok(0);
    }
    names.sort();
    for name in names {
        let dir = dir(&name)?;
        let started = read(&dir, "started").and_then(|t| t.parse::<u64>().ok());
        let ago = started
            .map(|t| ago(now().saturating_sub(t)))
            .unwrap_or_default();
        let cmd: String = read(&dir, "cmd")
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(70)
            .collect();
        println!("{name:<14} {:<22} {ago:<12} {cmd}", state(&dir));
    }
    Ok(0)
}

/// Лог задачи; с `follow` — дописывать новое, пока задача работает, и выйти с её кодом.
pub fn log(name: &str, follow: bool) -> io::Result<i32> {
    let dir = existing(name)?;
    let mut file = fs::File::open(dir.join("log"))?;
    let mut out = io::stdout().lock();
    loop {
        // Ещё работает — смотрим до чтения: строки, дописанные перед выходом, тоже попадут.
        let alive = running(&dir);
        io::copy(&mut file, &mut out)?;
        out.flush()?;
        if !follow {
            return Ok(0);
        }
        if !alive {
            eprintln!("ferry: задача {name}: {}", state(&dir));
            let code = read(&dir, "exit").and_then(|c| c.parse().ok());
            return Ok(code.unwrap_or(1));
        }
        thread::sleep(Duration::from_millis(300));
    }
}

pub fn kill(name: &str) -> io::Result<i32> {
    let dir = existing(name)?;
    if !running(&dir) {
        eprintln!("ferry: задача {name} уже не работает: {}", state(&dir));
        return Ok(0);
    }
    fs::write(dir.join("killed"), "")?;
    // Команда в своей группе (unix) или дереве (Windows); надсмотрщик запишет её код.
    // Если надсмотрщик ещё не успел её запустить — гасим его самого.
    let pid = read(&dir, "child").or_else(|| read(&dir, "pid"));
    // Код taskkill не показатель: в дереве вроде Chrome часть детей умирает сама, пока его
    // обходят, и он отвечает «не всё нашлось». Решает то, работает ли задача после.
    let killed = pid.and_then(|p| p.parse().ok()).map(kill_tree);
    for _ in 0..50 {
        if !running(&dir) {
            eprintln!("ferry: задача {name} остановлена");
            return Ok(0);
        }
        thread::sleep(Duration::from_millis(100));
    }
    let why = match killed {
        Some(Err(e)) => e.to_string(),
        _ => "всё ещё работает".into(),
    };
    Err(io::Error::other(format!(
        "задача {name} не остановилась: {why}"
    )))
}

/// Остановить все работающие задачи — часть `ferry stop`.
pub fn kill_all() -> io::Result<usize> {
    let Ok(entries) = fs::read_dir(crate::ferry_home()?.join("jobs")) else {
        return Ok(0);
    };
    let mut killed = 0;
    for entry in entries.flatten() {
        if running(&entry.path()) {
            kill(&entry.file_name().to_string_lossy())?;
            killed += 1;
        }
    }
    Ok(killed)
}

fn state(dir: &Path) -> String {
    match (read(dir, "killed"), read(dir, "exit")) {
        (Some(_), _) => "остановлена".into(),
        (None, Some(code)) => format!("завершилась, код {code}"),
        (None, None) if running(dir) => "работает".into(),
        // Надсмотрщик умер, не записав код: выход из системы, его убили вручную.
        (None, None) => "пропала".into(),
    }
}

fn running(dir: &Path) -> bool {
    if dir.join("exit").exists() {
        return false;
    }
    read(dir, "pid")
        .and_then(|p| p.parse().ok())
        .is_some_and(alive)
}

#[cfg(windows)]
fn alive(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    let out = Command::new("tasklist")
        .args(["/fi", &format!("PID eq {pid}"), "/fo", "csv", "/nh"])
        .creation_flags(crate::desk::CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Надсмотрщик — сам по себе: своя группа процессов, своя скрытая консоль на Windows.
/// И без унаследованных дескрипторов `ferry desk`: иначе вызов desk ждал бы его конца.
#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> isize;
        fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for which in [-10i32, -11, -12] {
        unsafe { SetHandleInformation(GetStdHandle(which as u32), HANDLE_FLAG_INHERIT, 0) };
    }
    command.creation_flags(crate::desk::CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    std::os::unix::process::CommandExt::process_group(command, 0);
}

fn dir(name: &str) -> io::Result<PathBuf> {
    Ok(crate::ferry_home()?.join("jobs").join(name))
}

fn existing(name: &str) -> io::Result<PathBuf> {
    let dir = dir(name)?;
    if dir.join("cmd").exists() {
        return Ok(dir);
    }
    Err(io::Error::other(format!(
        "нет задачи {name:?}; список — ferry jobs"
    )))
}

fn read(dir: &Path, file: &str) -> Option<String> {
    fs::read_to_string(dir.join(file))
        .ok()
        .map(|s| s.trim().to_string())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} с назад"),
        60..3600 => format!("{} мин назад", secs / 60),
        _ => format!("{} ч назад", secs / 3600),
    }
}
