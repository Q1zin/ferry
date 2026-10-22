use std::io;

mod agent_tools;
mod checks;
mod capture;
mod workflows;
mod devices;
mod diagnostics;
mod execution;
mod human;
mod mcp;
mod setup;
mod storage;

use ferry_core::mode::{self, Mode};
use ferry_core::{info, jobs, relay};

const USAGE: &str = "\
ferry — разработка и тесты на нескольких устройствах: на каждом — приложение ferry, команды
идут отсюда через приложение на этой машине

Устройства:
  ferry select [--os windows|macos] [--shell <имя>] [--tool <команда>]... [--desktop]
                                    JSON: подходящие связанные устройства и причины отказа; без брони
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

Проверки для агента (0.1.23):
  ferry check <профиль> [--devices local,WIN-11-2|all|os:windows] [--parallel 2]
              [--file ferry.checks.json] [--home] [--no-sync] [--isolated] [--id <id>] [--detach]
                                    build → launch → ready → test → collect → cleanup; JSON матрицы
  ferry check status|resume|collect|cancel <id> [--detach]
                                    читать / продолжить / собрать / отменить ту же операцию
  ferry diagnose [<устройство>] <задача> --run <id> [--window <окно>] [--junit <id>]
                                    отчёт запуска; --bundle [--artifact <id>]... — переносимые файлы
  ferry history [<устройство>] <задача> [--pin <run> | --unpin <run>]
  ferry history [<устройство>] <задача> [--older-than <дни>] [--max-bytes <байты>]
                                    место и preview очистки архивов; --apply <preview_token> удаляет
  ferry storage [<устройство>] [--kind caches|outputs|shots|recordings|checks|diagnostics|runners|questions|observations]
                [--only <entry_id>] [--older-than <дни>] [--max-bytes <байты>] [--apply <token>]
                                    общий учёт диска и preview; исходники не удаляются
  ferry act [<устройство>] --from <observe.json> --click <имя>|--key <клавиша>|--type <текст>
            --expect <точное имя элемента> [--timeout 10] [--dry-run]
                                    сверить окно и дерево, одно действие, проверить результат
  ferry act [<устройство>] --from <json> --click-locator <json>|--target <json> --key|--type <значение>
            --expect-json <json> [--expect-window <заголовок>] [--allow-already] [--dry-run]
                                    точная цель и её предки, проверка состояния или нового своего окна

Команды на устройствах:
  ferry run <цели> [--desk] [--no-sync] [--home] [--shell <имя>] [--] <команда>
                                    снимок проекта, затем команда в том же подкаталоге
                                    проекта на устройстве; --desk — с окнами (GUI, тесты),
                                    --home — в доме устройства, как вне репозитория;
                                    --shell: default, bash, powershell, cmd
  ferry sync <цели>                 рабочее дерево проекта → ~/ferry/<проект> на устройстве
  ferry doctor <цели>               готово ли устройство собирать этот проект: инструменты,
                                    доступ к git-зависимостям, файлы проекта
  ferry start <цели> <имя> [--no-sync] [--home] [--shell <имя>] [--] <команда>
                                    фоновая задача (приложение, dev-сервер, сборка): живёт
                                    после вызова, пишет лог; то же имя ещё раз — перезапуск
  ferry start <устройство|local> <имя> --request <id> [--session <id>] [--keep] [--timeout <сек>]
              [--headless] [--workspace <manifest.json>] [--home] [--shell <имя>] -- <команда>
                                    точный run_id, повтор запроса без повторного запуска
  ferry start-status <устройство|local> <request_id>
  ferry session begin [<id>] [--pid <PID_агента>] [--ttl <сек>]
  ferry session renew|show|end <id> [--ttl <сек>]
                                    срок работы агента; задачи и туннели убираются по концу сессии
  ferry workspace create [<устройство|local>] [--id <id>] [--root <проект>]
  ferry workspace diff <manifest.json> [--root <проект>]
  ferry workspace apply <manifest.json> --token <preview_token> [--root <проект>]
                                    изолированная копия, проверка и применение patch
  ferry jobs <цели> [--json]        задачи: работает, код выхода, когда запущена
  ferry log <устройство> <имя> [-f] лог задачи; -f — следить до конца и выйти с её кодом
  ferry result [<устройство>] <имя> [--wait <сек>] [--cursor <курсор>] [--max-bytes <байт>]
                                    JSON: состояние, код выхода и хвост лога (16 КиБ); курсор —
                                    только новые данные; --wait до 60 с, не останавливает задачу
  ferry runs [<устройство>] <имя>   последние 100 запусков с run_id; старые запуски сохраняются
  ferry artifacts [<устройство>] <имя> [--run <id>] [--add <файл> | --get <id> [--dest <каталог>]]
                                    список / копия файла в запуск / скачать; пути --add на устройстве
  ferry tests [<устройство>] <имя> --junit <id артефакта> [--run <id>]
                                    JSON: фактические testcase и до 50 падений из JUnit XML
  ferry result [<устройство>] <имя> --run <id>  результат сохранённого запуска, без --wait
  ferry kill <цели> <имя>           остановить задачу
  ferry stop <цели>                 снять всё: команды и задачи
  ferry put <цели> <файл>...        файлы отсюда — на то же место в проекте на устройстве
  ferry get <устройство> <путь> [куда]
                                    файл или каталог с устройства — сюда (~/… — от его дома)
  ferry pull <устройство>           правки, сделанные на устройстве, — сюда
  ferry tunnel [<устройство> <порт>] [--close] [--session <id>] [--keep]
                                    127.0.0.1:<порт> здесь → <порт> устройства (CDP,
                                    dev-сервер); без аргументов — открытые туннели
  ferry info <цели> [--json]        свойства: система, железо, питание, экран, ferry
  ferry power <цели> reboot|sleep|shutdown|cancel
                                    питание (Windows); reboot и shutdown — через 10 с
  ferry update [<цели>] --file <пакет>  локальная подписанная сборка (.sig рядом), ждать перезапуска
  ferry update [<цели>]             обновить ferry из релиза: на устройствах (all — и пульты) или
                                    (без целей) на этой машине; они перезапустятся и снова выйдут
                                    на связь
  ferry logs [<цели>] [-n <строк>]  лог приложения ferry на устройствах или (без целей) здесь:
                                    последние строки, по умолчанию 100
  ferry desk <устройство> [take [<минут>] [--note <намерение>] | free [--force]]
                                    бронь рабочего стола: окна и ввод одной сессии (агента или
                                    терминала) — любое действие само занимает стол на 3 мин, take —
                                    надольше (по умолчанию 30 мин), free — снять, --force — чужую
  ferry ask <устройство|local> <вопрос> [--id <id>] [--timeout 600] [--wait 0]
  ferry ask <устройство|local> --poll <id> [--wait 30] | --cancel <id>
                                    вопрос человеку; ответ done требует проверки результата
  ferry crashes <устройство> [<программа>] [--last]
                                    падения программ на устройстве, свежие сверху; --last —
                                    разбор последнего: исключение, стек (macOS — отчёты .ips,
                                    Windows — журнал «Приложение»)
  ferry trust [<хост>]              git с хоста на устройствах — через ключи этой машины
                                    (только чтение); без аргумента — доверенные хосты

Экран устройства (координаты — пиксели снимка ferry shot):
  ferry observe [<устройство>] [--window <окно>] [--ocr] [--structured] [--no-screenshot]
                [--element <locator.json> | --subtree <locator.json>] [--since <observe.json>]
                                    JSON: окно, фокус, элементы и путь снимка; OCR — координаты в снимке
  ferry shot <цели> [файл.png] [--window <окно> | --region x,y,ширина,высота]
                                    снимок экрана (по умолчанию ~/.ferry/shots/<устройство>.png);
                                    --window — само окно (процесс или часть заголовка), даже закрытое
                                    другими, --region — область; печатает, что прибавить к точке на
                                    снимке для click
  ferry windows <устройство>        окна сверху вниз: процесс, место, размер; * — фокус;
                                    свёрнутые и на других рабочих столах — «не на экране»
  ferry focus <устройство> <окно>   вывести окно (процесс или часть заголовка) вперёд, с фокусом,
                                    даже свёрнутое или с другого рабочего стола: окна задач ferry
                                    открываются позади
  ferry ui <устройство> [<окно>]    элементы окна с фокусом (или по процессу, части заголовка):
                                    роль, «имя», #id, место — по ним кликать надёжнее
  ferry click <устройство> <x> <y> [right|double] [--hold <комбинация>]
  ferry click <устройство> --name <имя|#id> [right|double] [--hold <комбинация>]
                                    клик в центр элемента с этим именем (или #id) в окне с фокусом;
                                    --hold shift — держать клавиши, пока жмём (shift/cmd+клик)
  ferry click <устройство> --text <текст> [right|double] [--hold <комбинация>]
                                    клик в центр текста на экране (распознавание) в окне с фокусом —
                                    для canvas, WebGL, игр без элементов
  ferry text <устройство> [<окно>]  текст окна с фокусом (или по процессу, части заголовка),
                                    распознанный с экрана: «текст» и место
  ferry move <устройство> <x> <y>   навести курсор (или --name <имя>)
  ferry drag <устройство> <x1> <y1> <x2> <y2> [--hold <комбинация>]
                                    перетащить левой кнопкой: выделение, ползунки, перенос
  ferry scroll <устройство> <n>     колесо: больше нуля — вверх
  ferry type <устройство> <текст>   ввести текст (юникод, раскладка не важна)
  ferry key <устройство> <комбинация>...
                                    клавиши: enter, ctrl+s, alt+tab, win+d, f5…
  ferry key <устройство> <комбинация> --for <секунд>
                                    держать и отпустить (долгое нажатие, до 60 с)
  ferry clip <устройство> [<текст>] буфер обмена: без текста — прочитать, с текстом — положить
  ferry wait <устройство> --window <окно> | --name <имя|#id> | --text <текст> | --port <порт>
                    | --log <задача> <текст> [--gone] [--timeout <секунд>]
                                    ждать, пока окно (процесс или часть заголовка), элемент или
                                    текст в окне с фокусом, порт устройства появится (с --gone —
                                    исчезнет), либо текст — в логе задачи; по умолчанию 30 с;
                                    дождался — печатает место или строку лога, нет — код 1
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
  ferry mcp                         MCP-сервер для Claude Code: claude mcp add ferry -- ferry mcp;
                                    снимки приходят в разговор картинками

Служебные: ferry ssh — ssh для git на устройстве (релей), ferry job <имя> <команда> (запуск
задачи), ferry job-supervise <имя> (надсмотрщик задачи).
";

fn managed_flags(mut args: &[&str]) -> bool {
    loop {
        match args {
            [
                "--request" | "--session" | "--keep" | "--timeout" | "--headless" | "--workspace",
                ..,
            ] => return true,
            ["--shell", _, rest @ ..] | ["--home" | "--no-sync" | "--close", rest @ ..] => {
                args = rest
            }
            _ => return false,
        }
    }
}

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
        ["desk", device] => devices::desk(device, "show", 0),
        ["desk", device, "take", rest @ ..] => human::desk_take(device, rest),
        ["ask", rest @ ..] => human::ask(rest),
        ["desk", device, "free"] => devices::desk(device, "free", 0),
        ["desk", device, "free", "--force"] => devices::desk(device, "force", 0),
        ["crashes", device] => devices::crashes(device, "", false),
        ["crashes", device, "--last"] => devices::crashes(device, "", true),
        ["crashes", device, process] => devices::crashes(device, process, false),
        ["crashes", device, process, "--last"] => devices::crashes(device, process, true),
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
        ["session", rest @ ..] => execution::session(rest),
        ["workspace", rest @ ..] => execution::workspace(rest),
        ["storage", rest @ ..] => storage::run(rest),
        ["start-status", rest @ ..] => execution::start_status(rest),
        ["start", who, _, ..]
            if !ferry_core::control::session().is_empty()
                && (*who == "all" || who.starts_with("os:") || who.contains(',')) =>
        {
            Err(io::Error::other(
                "start в сессии агента принимает одно устройство; для нескольких используй check",
            ))
        }
        ["start", _, _, rest @ ..]
            if managed_flags(rest) || !ferry_core::control::session().is_empty() =>
        {
            execution::start(&args[1..])
        }
        ["start", spec, name, rest @ ..] => start(spec, name, rest),
        // Раньше `jobs <цели>`: иначе `--tsv` приняли бы за имя устройства.
        ["jobs", "--tsv"] => jobs::list(true),
        ["jobs", spec, "--json"] => devices::jobs(spec, true),
        ["jobs", spec] => devices::jobs(spec, false),
        ["check", rest @ ..] => checks::run(rest),
        ["capture", rest @ ..] => capture::run(rest),
        ["workflow", rest @ ..] => workflows::run(rest),
        ["diagnose", rest @ ..] => diagnostics::run(rest),
        ["act", rest @ ..] => agent_tools::act(rest),
        ["history", rest @ ..] => agent_tools::history(rest),
        ["observe", rest @ ..] => agent_tools::observe(rest),
        ["select", rest @ ..] => agent_tools::select(rest),
        [kind @ ("runs" | "artifacts" | "tests"), rest @ ..] => agent_tools::evidence(kind, rest),
        ["result", rest @ ..] => job_result(rest),
        ["log", device, name] if *name != "-f" => devices::log(device, name, false),
        ["log", device, name, "-f"] => devices::log(device, name, true),
        ["kill", spec, name] => devices::kill(spec, name),
        ["tunnel"] => devices::tunnel(None, false),
        ["tunnel", _, _, rest @ ..]
            if managed_flags(rest) || !ferry_core::control::session().is_empty() =>
        {
            execution::tunnel(&args[1..])
        }
        ["tunnel", device, port] => devices::tunnel(Some((device, port)), false),
        ["tunnel", device, port, "--close"] => devices::tunnel(Some((device, port)), true),
        ["windows", device] => devices::input(device, owned(&["windows"])),
        ["focus", device, window @ ..] if !window.is_empty() => {
            devices::input(device, owned(&[&["focus"][..], window].concat()))
        }
        ["text", device, window @ ..] => {
            devices::input(device, owned(&[&["text"][..], window].concat()))
        }
        ["ui", device, window @ ..] => {
            devices::input(device, owned(&[&["ui"][..], window].concat()))
        }
        ["move", device, rest @ ..] if rest.len() >= 2 => {
            devices::input(device, owned(&[&["move"][..], rest].concat()))
        }
        ["drag", device, rest @ ..] if rest.len() >= 4 => {
            devices::input(device, owned(&[&["drag"][..], rest].concat()))
        }
        ["clip", device] => devices::input(device, owned(&["clip"])),
        ["clip", device, text @ ..] => {
            devices::input(device, owned(&["clip", text.join(" ").as_str()]))
        }
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
        ["update", "--file", file] => devices::update_file(None, file),
        ["update", spec, "--file", file] => devices::update_file(Some(spec), file),
        ["update"] => devices::update(None),
        ["update", spec] => devices::update(Some(spec)),
        ["skill"] => skill(),
        ["mcp"] => mcp::serve(),
        ["setup", rest @ ..] => setup::run(rest),
        ["init", rest @ ..] => setup::init(rest),
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

fn job_result(args: &[&str]) -> io::Result<i32> {
    let (device, name, mut rest) = match args {
        [device, name, rest @ ..] if !name.starts_with("--") => (Some(*device), *name, rest),
        [name, rest @ ..] => (None, *name, rest),
        _ => return usage(),
    };
    let (mut cursor, mut max_bytes, mut wait) = (None, jobs::DEFAULT_BYTES, 0);
    let mut run_id = None;
    while !rest.is_empty() {
        match rest {
            ["--run", value, tail @ ..] if run_id.is_none() => {
                run_id = Some(*value);
                rest = tail;
            }
            ["--cursor", value, tail @ ..] if cursor.is_none() => {
                cursor = Some(*value);
                rest = tail;
            }
            ["--max-bytes", value, tail @ ..] => {
                max_bytes = match value.parse::<u32>() {
                    Ok(n) if (4..=jobs::MAX_BYTES).contains(&n) => n,
                    _ => return usage(),
                };
                rest = tail;
            }
            ["--wait", value, tail @ ..] => {
                wait = match value.parse::<u32>() {
                    Ok(n) if n <= 60 => n,
                    _ => return usage(),
                };
                rest = tail;
            }
            _ => return usage(),
        }
    }
    if let Some(run) = run_id {
        if wait != 0 {
            return Err(io::Error::other(
                "для --run запрашивай результат без --wait",
            ));
        }
        return agent_tools::historical_result(device, name, run, cursor, max_bytes);
    }
    devices::job_result(device, name, cursor, max_bytes, wait)
}

/// `run <цели> [--desk] [--no-sync] [--] <команда>`: флаги — только до команды.
fn run(spec: &str, mut rest: &[&str]) -> io::Result<i32> {
    let mut shell = ferry_core::desk::Shell::Default;
    let (mut desk, mut sync, mut home) = (false, true, false);
    loop {
        match rest.first() {
            Some(&"--desk") => desk = true,
            Some(&"--no-sync") => sync = false,
            Some(&"--home") => home = true,
            Some(&"--shell") => {
                let value = rest.get(1).ok_or_else(|| {
                    io::Error::other("--shell: default, bash, powershell или cmd")
                })?;
                shell = value.parse()?;
                rest = &rest[1..];
            }
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
    devices::run(spec, desk, sync, home, &rest.join(" "), shell)
}

/// `start <цели> <имя> [--no-sync] [--] <команда>`.
fn start(spec: &str, name: &str, mut rest: &[&str]) -> io::Result<i32> {
    let mut shell = ferry_core::desk::Shell::Default;
    let (mut sync, mut home) = (true, false);
    loop {
        match rest.first() {
            Some(&"--no-sync") => sync = false,
            Some(&"--home") => home = true,
            Some(&"--shell") => {
                let value = rest.get(1).ok_or_else(|| {
                    io::Error::other("--shell: default, bash, powershell или cmd")
                })?;
                shell = value.parse()?;
                rest = &rest[1..];
            }
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
    devices::start(spec, name, sync, home, &rest.join(" "), shell)
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

/// `wait <устройство> (--window <окно> | --name <имя> | --port <порт> | --log <задача> <текст>)
/// [--gone] [--timeout <секунд>]`.
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
            ("--text", [text, tail @ ..]) => {
                what = Some(devices::Wait::Text(text));
                rest = tail;
            }
            ("--port", [port, tail @ ..]) => {
                let Ok(port) = port.parse() else {
                    return usage();
                };
                what = Some(devices::Wait::Port(port));
                rest = tail;
            }
            ("--log", [job, text, tail @ ..]) => {
                what = Some(devices::Wait::Log { job, text });
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
        // Текст в логе не «исчезает».
        Some(devices::Wait::Log { .. }) if gone => usage(),
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
