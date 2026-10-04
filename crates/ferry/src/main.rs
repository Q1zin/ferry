use std::io;

mod devices;

use ferry_core::mode::{self, Mode};
use ferry_core::{info, jobs, relay};

const USAGE: &str = "\
ferry — разработка и тесты на нескольких устройствах: на каждом — приложение ferry, команды
идут отсюда через приложение на этой машине

Устройства:
  ferry devices [--json]            это устройство и связанные: на связи ли, путь, роль, режим
  ferry pair                        показать код: это устройство ждёт, что его добавят
  ferry pair <код|ссылка>           связать с устройством, которое показало код
  ferry role [<устройство>] <роль>  роль этого или связанного устройства:
                                    both (узел и пульт), node (узел), console (пульт)
  ferry forget <устройство>         забыть связанное устройство здесь (на нём связь останется,
                                    пока не забудут и там)

<цели> — имя устройства, несколько через запятую, all — все на связи, os:windows — по системе.
На нескольких целях команда идёт параллельно, строки вывода — с [устройство], код выхода —
первый ненулевой. Отказ устройства по режиму или роли — код 75.

Команды на устройствах:
  ferry run <цели> [--desk] [--no-sync] [--home] [--] <команда>
                                    снимок проекта, затем команда в том же подкаталоге
                                    проекта на устройстве; --desk — с окнами (GUI, тесты),
                                    --home — в доме устройства, как вне репозитория
  ferry sync <цели>                 рабочее дерево проекта → ~/ferry/<проект> на устройстве
  ferry doctor <цели>               готово ли устройство собирать этот проект: инструменты,
                                    доступ к git-зависимостям, файлы проекта
  ferry start <цели> <имя> [--no-sync] [--home] [--] <команда>
                                    фоновая задача (приложение, dev-сервер, сборка): живёт
                                    после вызова, пишет лог; то же имя ещё раз — перезапуск
  ferry jobs <цели> [--json]        задачи: работает, код выхода, когда запущена
  ferry log <устройство> <имя> [-f] лог задачи; -f — следить до конца и выйти с её кодом
  ferry kill <цели> <имя>           остановить задачу
  ferry stop <цели>                 снять всё: команды и задачи
  ferry put <цели> <файл>...        файлы отсюда — на то же место в проекте на устройстве
  ferry get <устройство> <путь> [куда]
                                    файл или каталог с устройства — сюда (~/… — от его дома)
  ferry pull <устройство>           правки, сделанные на устройстве, — сюда
  ferry tunnel [<устройство> <порт>] [--close]
                                    127.0.0.1:<порт> здесь → <порт> устройства (CDP,
                                    dev-сервер); без аргументов — открытые туннели
  ferry info <цели> [--json]        свойства: система, железо, питание, экран, ferry
  ferry power <цели> reboot|sleep|shutdown|cancel
                                    питание (Windows); reboot и shutdown — через 10 с
  ferry update [<цели>]             обновить ferry из релиза: на устройствах (all — и пульты) или
                                    (без целей) на этой машине; они перезапустятся и снова выйдут
                                    на связь
  ferry logs [<цели>] [-n <строк>]  лог приложения ferry на устройствах или (без целей) здесь:
                                    последние строки, по умолчанию 100
  ferry trust [<хост>]              git с хоста на устройствах — через ключи этой машины
                                    (только чтение); без аргумента — доверенные хосты

Экран устройства (координаты — пиксели снимка ferry shot):
  ferry shot <цели> [файл.png] [--window <окно> | --region x,y,ширина,высота]
                                    снимок экрана (по умолчанию ~/.ferry/shots/<устройство>.png);
                                    --window — само окно (процесс или часть заголовка), даже закрытое
                                    другими, --region — область; печатает, что прибавить к точке на
                                    снимке для click
  ferry windows <устройство>        окна сверху вниз: процесс, место, размер; * — фокус
  ferry focus <устройство> <окно>   вывести окно (процесс или часть заголовка) вперёд, с фокусом:
                                    окна задач ferry открываются позади (только Windows)
  ferry ui <устройство> [<окно>]    элементы окна с фокусом (или по процессу, части заголовка):
                                    роль, «имя», #id, место — по ним кликать надёжнее
  ferry click <устройство> <x> <y> [right|double]
  ferry click <устройство> --name <имя|#id> [right|double]
                                    клик в центр элемента с этим именем (или #id) в окне с фокусом
  ferry move <устройство> <x> <y>   навести курсор (или --name <имя>)
  ferry scroll <устройство> <n>     колесо: больше нуля — вверх
  ferry type <устройство> <текст>   ввести текст (юникод, раскладка не важна)
  ferry key <устройство> <комбинация>...
                                    клавиши: enter, ctrl+s, alt+tab, win+d, f5…
  ferry wait <устройство> --window <окно> | --name <имя|#id> [--gone] [--timeout <секунд>]
                                    ждать, пока окно (процесс или часть заголовка) или элемент
                                    окна с фокусом появится (с --gone — исчезнет); по умолчанию
                                    30 с; появилось — печатает его место, не дождался — код 1
  ferry rec <устройство> <секунды> [кадр/с]
                                    запись кадрами (5 в секунду) → ~/.ferry/recs/…;
                                    печатает лист-обзор, с ffmpeg здесь — ещё и mp4

На самой машине:
  ferry info                        свойства этой машины (ключ=значение)
  ferry mode [active|no-desktop|paused]
                                    режим: работать, не трогать рабочий стол, отключиться
  ferry jobs | log <имя> [-f] | kill <имя>
                                    фоновые задачи этой машины
  ferry skill                       поставить скилл для Claude Code (~/.claude/skills/ferry):
                                    как работать с устройствами через ferry

Служебные: ferry ssh — ssh для git на устройстве (релей), ferry job <имя> <команда> (запуск
задачи), ferry job-supervise <имя> (надсмотрщик задачи).
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["devices"] => devices::list(false),
        ["devices", "--json"] => devices::list(true),
        ["pair"] => devices::pair(None),
        ["pair", code] => devices::pair(Some(code)),
        ["role", role] => devices::role(None, role),
        ["role", device, role] => devices::role(Some(device), role),
        ["forget", device] => devices::forget(device),
        ["info", spec] => devices::info(spec, false),
        ["info", spec, "--json"] => devices::info(spec, true),
        ["sync", spec] => devices::sync(spec),
        ["doctor", spec] => devices::doctor(spec),
        ["run", spec, rest @ ..] => run(spec, rest),
        ["pull", device] => devices::pull(device),
        ["put", spec, paths @ ..] if !paths.is_empty() => devices::put(spec, paths),
        ["get", device, path] => devices::get(device, path, None),
        ["get", device, path, dest] => devices::get(device, path, Some(dest)),
        ["shot", spec, rest @ ..] => shot(spec, rest),
        ["wait", device, rest @ ..] => wait(device, rest),
        ["stop", spec] => devices::stop(spec),
        ["start", spec, name, rest @ ..] => start(spec, name, rest),
        // Раньше `jobs <цели>`: иначе `--tsv` приняли бы за имя устройства.
        ["jobs", "--tsv"] => jobs::list(true),
        ["jobs", spec, "--json"] => devices::jobs(spec, true),
        ["jobs", spec] => devices::jobs(spec, false),
        ["log", device, name] if *name != "-f" => devices::log(device, name, false),
        ["log", device, name, "-f"] => devices::log(device, name, true),
        ["kill", spec, name] => devices::kill(spec, name),
        ["tunnel"] => devices::tunnel(None, false),
        ["tunnel", device, port] => devices::tunnel(Some((device, port)), false),
        ["tunnel", device, port, "--close"] => devices::tunnel(Some((device, port)), true),
        ["windows", device] => devices::input(device, owned(&["windows"])),
        ["focus", device, window @ ..] if !window.is_empty() => {
            devices::input(device, owned(&[&["focus"][..], window].concat()))
        }
        ["ui", device, window @ ..] => {
            devices::input(device, owned(&[&["ui"][..], window].concat()))
        }
        ["move", device, x, y] => devices::input(device, owned(&["move", x, y])),
        ["click", device, x, y, how @ ..] => {
            devices::input(device, owned(&[&["click", *x, *y][..], how].concat()))
        }
        ["scroll", device, clicks] => devices::input(device, owned(&["scroll", clicks])),
        ["type", device, text @ ..] if !text.is_empty() => {
            devices::input(device, owned(&["type", text.join(" ").as_str()]))
        }
        ["key", device, combos @ ..] if !combos.is_empty() => {
            devices::input(device, owned(&[&["key"][..], combos].concat()))
        }
        ["rec", device, seconds] => devices::rec(device, seconds, "5"),
        ["rec", device, seconds, fps] => devices::rec(device, seconds, fps),
        ["power", spec, action] => devices::power(spec, action),
        ["logs", rest @ ..] => logs(rest),
        ["update"] => devices::update(None),
        ["update", spec] => devices::update(Some(spec)),
        ["skill"] => skill(),
        ["trust"] => relay::trust(None),
        ["trust", host] => relay::trust(Some(host)),
        ["ssh", args @ ..] => relay::ssh(args),
        ["info"] => info::command(),
        ["version" | "--version"] => {
            println!("ferry {}", ferry_core::VERSION);
            Ok(0)
        }
        ["mode"] => mode::command(None),
        ["mode", name] if Mode::parse(name).is_some() => mode::command(Some(name)),
        ["job", name, cmd @ ..] if !cmd.is_empty() => jobs::start(name, &cmd.join(" ")),
        ["job-supervise", name] => jobs::supervise(name),
        ["jobs"] => jobs::list(false),
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

/// `run <цели> [--desk] [--no-sync] [--] <команда>`: флаги — только до команды.
fn run(spec: &str, mut rest: &[&str]) -> io::Result<i32> {
    let (mut desk, mut sync, mut home) = (false, true, false);
    loop {
        match rest.first() {
            Some(&"--desk") => desk = true,
            Some(&"--no-sync") => sync = false,
            Some(&"--home") => home = true,
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
    devices::run(spec, desk, sync, home, &rest.join(" "))
}

/// `start <цели> <имя> [--no-sync] [--] <команда>`.
fn start(spec: &str, name: &str, mut rest: &[&str]) -> io::Result<i32> {
    let (mut sync, mut home) = (true, false);
    loop {
        match rest.first() {
            Some(&"--no-sync") => sync = false,
            Some(&"--home") => home = true,
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
    devices::start(spec, name, sync, home, &rest.join(" "))
}

/// `shot <цели> [файл.png] [--window <окно>] [--region x,y,ширина,высота]`.
fn shot(spec: &str, mut rest: &[&str]) -> io::Result<i32> {
    let (mut file, mut area) = (None, devices::Area::Screen);
    while let Some((first, tail)) = rest.split_first() {
        match (*first, tail) {
            ("--window", [name, tail @ ..]) => {
                area = devices::Area::Window(name);
                rest = tail;
            }
            ("--region", [region, tail @ ..]) => {
                let numbers: Vec<i32> = region
                    .split(',')
                    .filter_map(|n| n.trim().parse().ok())
                    .collect();
                let Ok(region) = numbers.try_into() else {
                    return usage();
                };
                area = devices::Area::Region(region);
                rest = tail;
            }
            (path, tail) if file.is_none() && !path.starts_with("--") => {
                file = Some(path);
                rest = tail;
            }
            _ => return usage(),
        }
    }
    devices::shot(spec, file, area)
}

/// `wait <устройство> (--window <окно> | --name <имя>) [--gone] [--timeout <секунд>]`.
fn wait(device: &str, mut rest: &[&str]) -> io::Result<i32> {
    let (mut what, mut gone, mut timeout) = (None, false, 30);
    while let Some((first, tail)) = rest.split_first() {
        match (*first, tail) {
            ("--window", [name, tail @ ..]) => {
                what = Some(devices::Wait::Window(name));
                rest = tail;
            }
            ("--name", [name, tail @ ..]) => {
                what = Some(devices::Wait::Name(name));
                rest = tail;
            }
            ("--gone", tail) => {
                gone = true;
                rest = tail;
            }
            ("--timeout", [seconds, tail @ ..]) => {
                let Ok(seconds) = seconds.parse() else {
                    return usage();
                };
                timeout = seconds;
                rest = tail;
            }
            _ => return usage(),
        }
    }
    match what {
        Some(what) => devices::wait(device, what, gone, std::time::Duration::from_secs(timeout)),
        None => usage(),
    }
}

/// `logs [<цели>] [-n <строк>]`.
fn logs(rest: &[&str]) -> io::Result<i32> {
    let (spec, lines) = match rest {
        [] => (None, "100"),
        ["-n", n] => (None, *n),
        [spec] => (Some(*spec), "100"),
        [spec, "-n", n] => (Some(*spec), *n),
        _ => return usage(),
    };
    match lines.parse() {
        Ok(lines) => devices::logs(spec, lines),
        Err(_) => usage(),
    }
}

/// `ferry skill`: скилл едет внутри CLI — ставится туда, где его найдёт Claude Code.
fn skill() -> io::Result<i32> {
    let dir = ferry_core::home()?.join(".claude/skills/ferry");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, include_str!("../../../skills/ferry/SKILL.md"))?;
    println!("ferry: скилл для Claude Code — {}", path.display());
    Ok(0)
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

fn usage() -> io::Result<i32> {
    eprint!("{USAGE}");
    Ok(2)
}
