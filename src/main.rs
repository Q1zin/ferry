mod desk;
mod doctor;
mod hub;
mod jobs;
mod project;
mod relay;
mod screen;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

const USAGE: &str = "\
ferry — разработка на нескольких машинах: этот компьютер — хаб, остальные — узлы

На хабе:
  ferry add <узел> <user@адрес>     записать узел и проверить связь
  ferry ls                          узлы: на связи ли, ОС, отвечает ли рабочий стол
  ferry doctor <узел>               готов ли узел собирать этот проект: инструменты,
                                    доступ к git-зависимостям, файлы проекта
  ferry sync <узел>                 рабочее дерево проекта → ~/ferry/<проект> на узле
  ferry run <узел> [--desk] [--no-sync] [--] <команда>
                                    sync, затем команда в том же подкаталоге проекта на узле;
                                    --desk — на рабочем столе узла (окна, фокус, GUI-тесты)
  ferry pull <узел>                 правки, сделанные на узле, — сюда
  ferry put <узел> <файл>...        файлы отсюда — на то же место в проекте на узле
  ferry get <узел> <путь> [куда]    файл или каталог с узла — сюда (~/… — от дома узла)
  ferry shot <узел> [файл.png]      снимок экрана узла (по умолчанию ~/.ferry/shots/<узел>.png)
  ferry start <узел> <имя> [--no-sync] [--] <команда>
                                    фоновая задача на рабочем столе узла (приложение,
                                    dev-сервер, сборка): живёт после вызова, пишет лог;
                                    то же имя ещё раз — перезапуск
  ferry jobs <узел>                 задачи узла: работает, код выхода, когда запущена
  ferry log <узел> <имя> [-f]       лог задачи; -f — следить до конца и выйти с её кодом
  ferry kill <узел> <имя>           остановить задачу
  ferry tunnel [<узел> <порт>] [--close]
                                    127.0.0.1:<порт> здесь → <порт> узла (CDP, dev-сервер);
                                    без аргументов — открытые туннели
  ferry stop <узел>                 снять всё: команды --desk и фоновые задачи

Экран узла (координаты — пиксели снимка ferry shot):
  ferry windows <узел>              окна сверху вниз: процесс, место, размер; * — фокус
  ferry click <узел> <x> <y> [right|double]
  ferry move <узел> <x> <y>         навести курсор
  ferry scroll <узел> <n>           колесо: больше нуля — вверх
  ferry type <узел> <текст>         ввести текст (юникод, раскладка не важна)
  ferry key <узел> <комбинация>...  клавиши: enter, ctrl+s, alt+tab, win+d, f5…
  ferry rec <узел> <секунды> [кадр/с]
                                    запись кадрами (5 в секунду) → ~/.ferry/recs/…;
                                    печатает лист-обзор, с ffmpeg на хабе — ещё и mp4
  ferry update <узел> <ferry.exe>   заменить ferry на узле и перезапустить демон
  ferry restart <узел>              перезапустить демон (подхватит новый PATH)
  ferry trust [<хост>]              git с хоста на узлах — через ключи этого хаба (только
                                    чтение); без аргумента — список доверенных хостов

На узле:
  ferry daemon                      принимать команды рабочего стола (запускает планировщик)
  ferry desk <команда>              выполнить команду на рабочем столе этой машины
  ferry screenshot <файл.png>       снимок экрана этой машины (запускать через desk)
  ferry record <секунды> <кадр/с> <каталог>
                                    запись кадрами (через desk)
  ferry input windows | move | click | scroll | type | key …
                                    окна и ввод на этой машине (через desk)
  ferry jobs | log <имя> [-f] | kill <имя>
                                    задачи этой машины
  ferry stop                        снять все команды рабочего стола и задачи

Служебные: ferry relay — релей git на хабе; на узле — ferry ssh (ssh для git),
ferry job <имя> <команда> (запуск задачи), ferry job-supervise <имя> (надсмотрщик задачи).
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
        ["put", host, paths @ ..] if !paths.is_empty() => hub::put(host, paths),
        ["get", host, path] => hub::get(host, path, None),
        ["get", host, path, dest] => hub::get(host, path, Some(dest)),
        ["shot", host] => hub::shot(host, None),
        ["shot", host, file] => hub::shot(host, Some(file)),
        ["stop", host] => hub::stop(host),
        ["doctor", host] => hub::doctor(host),
        ["start", host, name, rest @ ..] => start(host, name, rest),
        ["jobs", host] => hub::jobs(host),
        ["log", host, name] if *name != "-f" => hub::log(host, name, false),
        ["log", host, name, "-f"] => hub::log(host, name, true),
        ["kill", host, name] => hub::kill(host, name),
        ["tunnel"] => hub::tunnel(None, false),
        ["tunnel", host, port] => hub::tunnel(Some((host, port)), false),
        ["tunnel", host, port, "--close"] => hub::tunnel(Some((host, port)), true),
        ["windows", host] => hub::input(host, &["windows"]),
        ["move", host, x, y] => hub::input(host, &["move", x, y]),
        ["click", host, x, y, how @ ..] => {
            hub::input(host, &[&["click", *x, *y][..], how].concat())
        }
        ["scroll", host, clicks] => hub::input(host, &["scroll", clicks]),
        ["type", host, text @ ..] if !text.is_empty() => {
            hub::input(host, &["type", text.join(" ").as_str()])
        }
        ["key", host, combos @ ..] if !combos.is_empty() => {
            hub::input(host, &[&["key"][..], combos].concat())
        }
        ["rec", host, seconds] => hub::rec(host, seconds, "5"),
        ["rec", host, seconds, fps] => hub::rec(host, seconds, fps),
        ["update", host, file] => hub::update(host, file),
        ["restart", host] => hub::restart(host),
        ["trust"] => relay::trust(None),
        ["trust", host] => relay::trust(Some(host)),
        ["relay"] => relay::serve(),
        ["ssh", args @ ..] => relay::ssh(args),
        ["daemon"] => desk::daemon(),
        // Аргументы склеиваются через пробел и уходят в shell одной строкой, как у ssh.
        ["desk", cmd @ ..] if !cmd.is_empty() => desk::run(&cmd.join(" ")),
        ["screenshot", file] => screen::screenshot(file),
        ["record", seconds, fps, dir] => screen::record(seconds, fps, dir),
        ["input", rest @ ..] => screen::input(rest),
        ["stop"] => desk::stop(),
        ["job", name, cmd @ ..] if !cmd.is_empty() => jobs::start(name, &cmd.join(" ")),
        ["job-supervise", name] => jobs::supervise(name),
        ["jobs"] => jobs::list(),
        ["log", name] => jobs::log(name, false),
        ["log", name, "-f"] => jobs::log(name, true),
        ["kill", name] => jobs::kill(name),
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

/// `start <узел> <имя> [--no-sync] [--] <команда>`.
fn start(host: &str, name: &str, mut rest: &[&str]) -> io::Result<i32> {
    let mut sync = true;
    loop {
        match rest.first() {
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
    hub::start(host, name, sync, &rest.join(" "))
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

/// Строка в одинарных кавычках для POSIX-шелла узла (там Git Bash или zsh).
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(255)
}
