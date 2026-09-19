mod desk;
mod hub;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

const USAGE: &str = "\
ferry — разработка на нескольких машинах: этот компьютер — хаб, остальные — узлы

На хабе:
  ferry add <узел> <user@адрес>     записать узел и проверить связь
  ferry ls                          узлы: на связи ли, ОС, отвечает ли рабочий стол
  ferry sync <узел>                 рабочее дерево проекта → ~/ferry/<проект> на узле
  ferry run <узел> [--desk] [--no-sync] [--] <команда>
                                    sync, затем команда в том же подкаталоге проекта на узле;
                                    --desk — на рабочем столе узла (окна, фокус, GUI-тесты)
  ferry pull <узел>                 правки, сделанные на узле, — сюда
  ferry shot <узел> [файл.png]      снимок экрана узла (по умолчанию ~/.ferry/shots/<узел>.png)
  ferry stop <узел>                 снять все команды, запущенные через --desk
  ferry update <узел> <ferry.exe>   заменить ferry на узле и перезапустить демон

На узле:
  ferry daemon                      принимать команды рабочего стола (запускает планировщик)
  ferry desk <команда>              выполнить команду на рабочем столе этой машины
  ferry screenshot <файл.png>       снимок экрана этой машины (запускать через desk)
  ferry stop                        снять все команды рабочего стола
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["add", name, target] => hub::add(name, target),
        ["ls"] => hub::ls(),
        ["sync", host] => hub::sync(host),
        ["run", host, rest @ ..] => run(host, rest),
        ["pull", host] => hub::pull(host),
        ["shot", host] => hub::shot(host, None),
        ["shot", host, file] => hub::shot(host, Some(file)),
        ["stop", host] => hub::stop(host),
        ["update", host, file] => hub::update(host, file),
        ["daemon"] => desk::daemon(),
        // Аргументы склеиваются через пробел и уходят в shell одной строкой, как у ssh.
        ["desk", cmd @ ..] if !cmd.is_empty() => desk::run(&cmd.join(" ")),
        ["screenshot", file] => desk::screenshot(file),
        ["stop"] => desk::stop(),
        _ => usage(),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("ferry: {e}");
            std::process::exit(255)
        }
    }
}

/// `run <узел> [--desk] [--no-sync] [--] <команда>`: флаги — только до команды.
fn run(host: &str, mut rest: &[&str]) -> io::Result<i32> {
    let (mut desk, mut sync) = (false, true);
    loop {
        match rest.first() {
            Some(&"--desk") => desk = true,
            Some(&"--no-sync") => sync = false,
            Some(&"--") => {
                rest = &rest[1..];
                break;
            }
            _ => break,
        }
        rest = &rest[1..];
    }
    if rest.is_empty() {
        return usage();
    }
    hub::run(host, desk, sync, &rest.join(" "))
}

fn usage() -> io::Result<i32> {
    eprint!("{USAGE}");
    Ok(2)
}

fn home() -> io::Result<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(var).ok_or_else(|| io::Error::other(format!("не задан {var}")))?;
    Ok(home.into())
}

/// `~/.ferry`: на узле — демон и его бинарник, на хабе — узлы, снимки экрана, патчи.
fn ferry_home() -> io::Result<PathBuf> {
    Ok(home()?.join(".ferry"))
}

/// Записать файл, доступный только владельцу (на Windows профиль и так закрыт).
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(text.as_bytes())
}

fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(255)
}
