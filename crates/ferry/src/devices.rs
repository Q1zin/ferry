//! Устройства — через приложение ferry на этой машине: оно держит связь, CLI его спрашивает.
//!
//! Цели команд: имя устройства (или начало ключа), несколько через запятую, `all` — все на
//! связи, которые принимают команды, `os:windows` / `os:macos` — по системе. На нескольких
//! целях команда идёт параллельно, строки вывода подписаны `[устройство]`, код выхода — первый
//! ненулевой по порядку целей.

mod grouped;

use ferry_core::desk::Shell;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferry_core::control::{self, Request, Response};
use ferry_core::device::{Dir, Fail, PeerView, Role, Snapshot};
use ferry_core::sync;

pub(crate) fn ask(request: Request) -> Result<Response, Fail> {
    match control::call(request)? {
        Response::Error(fail) => Err(fail),
        response => Ok(response),
    }
}

fn unexpected() -> Fail {
    Fail::new("приложение ответило не то — версии приложения и CLI разные?")
}

pub(crate) fn state() -> Result<Snapshot, Fail> {
    match ask(Request::State)? {
        Response::State(snap) => Ok(snap),
        _ => Err(unexpected()),
    }
}

/// Ошибка устройства — сообщением и её кодом (75 — отказ по режиму или роли).
pub(crate) fn finish(result: Result<i32, Fail>) -> io::Result<i32> {
    Ok(result.unwrap_or_else(|fail| {
        eprintln!("ferry: {fail}");
        fail.code
    }))
}

// --- Цели ------------------------------------------------------------------------------------

/// Имена устройств по спецификации целей.
/// `any_role` — и пульты тоже: обновлять нужно всех, а команды пульт не принимает.
fn targets(spec: &str, any_role: bool) -> Result<Vec<String>, Fail> {
    let snap = state()?;
    let mut names = Vec::new();
    let mut skipped = Vec::new();
    let accepts =
        |p: &PeerView| any_role || p.status.as_ref().is_some_and(|s| s.role.accepts_commands());
    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let matched: Vec<&PeerView> = match item {
            "all" => snap.peers.iter().collect(),
            os if os.starts_with("os:") => {
                let want = &os[3..];
                snap.peers
                    .iter()
                    .filter(|p| p.status.as_ref().is_some_and(|s| s.os == want))
                    .collect()
            }
            who => vec![snap.find(who).map_err(Fail::new)?],
        };
        let group = item == "all" || item.starts_with("os:");
        for peer in matched {
            if group && !(peer.online && accepts(peer)) {
                skipped.push(peer.name.clone());
            } else if !names.contains(&peer.name) {
                names.push(peer.name.clone());
            }
        }
    }
    if !skipped.is_empty() {
        eprintln!(
            "ferry: пропущены (не на связи или пульт): {}",
            skipped.join(", ")
        );
    }
    if names.is_empty() {
        return Err(Fail::new(format!(
            "под «{spec}» нет устройств на связи — ferry devices"
        )));
    }
    Ok(names)
}

fn filtered_targets(spec: &str, filter: Option<&str>) -> Result<Vec<String>, Fail> {
    let names = targets(spec, false)?;
    if names.len() > 64 {
        return Err(Fail::new("run: не более 64 устройств"));
    }
    let Some(filter) = filter else {
        return Ok(names);
    };
    let selected = targets(filter, false)?;
    let names: Vec<_> = names
        .into_iter()
        .filter(|name| selected.contains(name))
        .collect();
    if names.is_empty() {
        return Err(Fail::new("--device не совпадает с выбранными устройствами"));
    }
    Ok(names)
}
/// Вывод одной цели: как есть или строками с подписью устройства.
struct Printer {
    prefix: Option<String>,
    out: Mutex<Vec<u8>>,
    err: Mutex<Vec<u8>>,
    spool: Option<Mutex<grouped::Spool>>,
}

/// Строки разных устройств не перемешиваются внутри строки.
static LINES: Mutex<()> = Mutex::new(());

impl Printer {
    fn new(prefix: Option<&str>) -> Printer {
        Printer {
            prefix: prefix.map(|p| format!("[{p}] ")),
            out: Mutex::default(),
            err: Mutex::default(),
            spool: None,
        }
    }

    fn spooled(path: &Path) -> io::Result<Self> {
        Ok(Self {
            prefix: None,
            out: Mutex::default(),
            err: Mutex::default(),
            spool: Some(Mutex::new(grouped::Spool::new(path)?)),
        })
    }
    fn check(&self) -> Result<(), Fail> {
        self.spool
            .as_ref()
            .map_or(Ok(()), |spool| spool.lock().unwrap().check())
    }

    fn out(&self, data: &[u8]) {
        self.write(data, false);
    }

    fn err(&self, data: &[u8]) {
        self.write(data, true);
    }

    fn line(&self, text: &str, err: bool) {
        self.write(format!("{}\n", text.trim_end()).as_bytes(), err);
    }

    fn write(&self, data: &[u8], err: bool) {
        if let Some(spool) = &self.spool {
            spool.lock().unwrap().write(data, err);
            return;
        }
        let Some(prefix) = &self.prefix else {
            let _ = if err {
                io::stderr().write_all(data)
            } else {
                io::stdout().write_all(data)
            };
            let _ = if err {
                io::stderr().flush()
            } else {
                io::stdout().flush()
            };
            return;
        };
        let mut buffer = if err {
            self.err.lock().unwrap()
        } else {
            self.out.lock().unwrap()
        };
        buffer.extend_from_slice(data);
        while let Some(end) = buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            emit(prefix, &line, err);
        }
        // Commands without line breaks must not retain an unbounded buffer.
        if buffer.len() >= 16384 {
            emit(prefix, &buffer, err);
            buffer.clear();
        }
    }

    /// Хвост без перевода строки — в конце.
    fn flush(&self) {
        let Some(prefix) = &self.prefix else { return };
        for (buffer, err) in [(&self.out, false), (&self.err, true)] {
            let mut buffer = buffer.lock().unwrap();
            if !buffer.is_empty() {
                buffer.push(b'\n');
                emit(prefix, &buffer, err);
                buffer.clear();
            }
        }
    }
}

fn emit(prefix: &str, line: &[u8], err: bool) {
    let _guard = LINES.lock().unwrap();
    let mut joined = prefix.as_bytes().to_vec();
    joined.extend_from_slice(line);
    let _ = if err {
        io::stderr().write_all(&joined)
    } else {
        io::stdout().write_all(&joined)
    };
}

/// Выполнить на каждой цели параллельно. Одна цель — вывод как есть.
fn each(spec: &str, job: impl Fn(&str, &Printer) -> Result<i32, Fail> + Sync) -> io::Result<i32> {
    each_of(spec, false, job)
}

fn each_of(
    spec: &str,
    any_role: bool,
    job: impl Fn(&str, &Printer) -> Result<i32, Fail> + Sync,
) -> io::Result<i32> {
    let names = match targets(spec, any_role) {
        Ok(names) => names,
        Err(fail) => return finish(Err(fail)),
    };
    each_names(&names, job)
}
fn each_names(
    names: &[String],
    job: impl Fn(&str, &Printer) -> Result<i32, Fail> + Sync,
) -> io::Result<i32> {
    if let [name] = names {
        return finish(job(name, &Printer::new(None)));
    }
    let codes: Vec<i32> = std::thread::scope(|s| {
        let runs: Vec<_> = names
            .iter()
            .map(|name| {
                let job = &job;
                s.spawn(move || {
                    let printer = Printer::new(Some(name));
                    let code = job(name, &printer).unwrap_or_else(|fail| {
                        printer.line(&format!("ferry: {fail}"), true);
                        fail.code
                    });
                    printer.flush();
                    code
                })
            })
            .collect();
        runs.into_iter()
            .map(|run| run.join().unwrap_or(1))
            .collect()
    });
    let summary: Vec<String> = names
        .iter()
        .zip(&codes)
        .map(|(name, code)| format!("{name} — {code}"))
        .collect();
    eprintln!("ferry: {}", summary.join(", "));
    Ok(codes.into_iter().find(|&c| c != 0).unwrap_or(0))
}

// --- Проект ----------------------------------------------------------------------------------

/// Репозиторий, где мы, и его снимок (если нужен sync).
pub(crate) struct Project {
    pub(crate) repo: sync::Repo,
    pub(crate) snapshot: Option<(String, String)>,
}

/// `home` — как вне репозитория: без снимка, в доме устройства.
pub(crate) fn project(snapshot: bool, home: bool) -> Result<Option<Project>, Fail> {
    if home {
        return Ok(None);
    }
    let Some(repo) = sync::repo()? else {
        return Ok(None);
    };
    let snapshot = if snapshot {
        Some(sync::snapshot(&repo)?)
    } else {
        None
    };
    Ok(Some(Project { repo, snapshot }))
}

/// Где выполнять на устройстве: то же место в проекте или дом, если мы не в репозитории.
pub(crate) fn dir(project: &Option<Project>) -> Dir {
    match project {
        Some(p) => Dir::Project {
            name: p.repo.name.clone(),
            prefix: p.repo.prefix.clone(),
        },
        None => Dir::Home,
    }
}

fn sync_one(device: &str, project: &Project, printer: &Printer) -> Result<(), Fail> {
    let Some((commit, tree)) = &project.snapshot else {
        return Ok(());
    };
    let request = Request::Sync {
        device: device.into(),
        root: project.repo.root.clone(),
        project: project.repo.name.clone(),
        commit: commit.clone(),
        tree: tree.clone(),
    };
    match ask(request)? {
        Response::Message(said) => {
            printer.line(&said, true);
            Ok(())
        }
        _ => Err(unexpected()),
    }
}

/// Поток вывода `Run` / `Log`: куски — как есть, в конце код выхода.
fn stream(request: Request, printer: &Printer) -> Result<i32, Fail> {
    for response in control::open(request)? {
        match response? {
            Response::Out(data) => {
                printer.out(&data);
                printer.check()?;
            }
            Response::Err(data) => {
                printer.err(&data);
                printer.check()?;
            }
            Response::Exit(code) => return Ok(code),
            Response::Error(fail) => return Err(fail),
            _ => return Err(unexpected()),
        }
    }
    Err(Fail::new("приложение оборвало ответ"))
}

/// Ответ-сообщение — в stderr: stdout остаётся для данных.
fn said(response: Response, printer: &Printer) -> Result<i32, Fail> {
    match response {
        Response::Message(text) => {
            if !text.trim().is_empty() {
                printer.line(&text, true);
            }
            Ok(0)
        }
        Response::Done => Ok(0),
        _ => Err(unexpected()),
    }
}

// --- Команды ---------------------------------------------------------------------------------

/// `ferry devices`: это устройство и связанные.
pub fn list(json: bool) -> io::Result<i32> {
    finish(state().map(|snap| {
        if json {
            let view = serde_json::json!({ "id": snap.id, "me": snap.me, "peers": snap.peers });
            println!("{view}");
        } else {
            print!("{}", table(&snap));
        }
        0
    }))
}

fn table(snap: &Snapshot) -> String {
    let me = &snap.me;
    let mut rows = vec![[
        me.name.clone(),
        "это устройство".into(),
        os(&me.os).into(),
        me.role.label().into(),
        me.mode.label().into(),
    ]];
    for p in &snap.peers {
        rows.push(match &p.status {
            Some(s) => [
                p.name.clone(),
                path(p),
                os(&s.os).into(),
                s.role.label().into(),
                s.mode.label().into(),
            ],
            None => [
                p.name.clone(),
                path(p),
                String::new(),
                String::new(),
                String::new(),
            ],
        });
    }
    let widths: Vec<usize> = (0..5)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in &rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell}{}", " ".repeat(w - cell.chars().count())))
            .collect();
        out += &format!("  {}\n", cells.join("   ").trim_end());
    }
    if snap.peers.is_empty() {
        out += "\nСвязанных устройств нет: ferry pair — показать код здесь, ferry pair <код> — ввести код с другого.\n";
    }
    out
}

fn path(p: &PeerView) -> String {
    if !p.online {
        return "не на связи".into();
    }
    let how = if p.direct == Some(false) {
        "через relay"
    } else {
        "напрямую"
    };
    match p.rtt_ms {
        Some(ms) => format!("{how}, {ms} мс"),
        None => how.into(),
    }
}

fn os(name: &str) -> &str {
    match name {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    }
}

/// `ferry pair` — показать код; `ferry pair <код|ссылка>` — связать с тем, кто его показал.
pub fn pair(code: Option<&str>) -> io::Result<i32> {
    finish((|| {
        match code {
            None => {
                let Response::Pairing(info) = ask(Request::OpenPairing)? else {
                    return Err(unexpected());
                };
                println!(
                    "Код: {}  (работает {} мин)",
                    info.code,
                    info.seconds_left.div_ceil(60)
                );
                println!(
                    "Введи его на другом устройстве: ferry pair {} — или в приложении.",
                    info.code
                );
                println!("Из другой сети — ссылка целиком:\n  {}", info.ticket);
            }
            Some(code) => {
                let Response::Peer(peer) = ask(Request::Pair { code: code.into() })? else {
                    return Err(unexpected());
                };
                println!("Связано: {} ({})", peer.name, path(&peer));
            }
        }
        Ok(0)
    })())
}

/// `ferry role [<устройство>] <роль>`.
pub fn role(device: Option<&str>, role: &str) -> io::Result<i32> {
    finish((|| {
        let role = Role::parse(role).ok_or_else(|| {
            Fail::new(format!(
                "роли: both (узел и пульт), node (узел), console (пульт) — а не {role:?}"
            ))
        })?;
        ask(Request::SetRole {
            device: device.map(String::from),
            role,
        })?;
        println!("{}: {}", device.unwrap_or("это устройство"), role.label());
        Ok(0)
    })())
}

/// `ferry desk <устройство> [take [минут] | free [--force]]`: бронь рабочего стола устройства.
pub fn desk(device: &str, action: &str, minutes: u32) -> io::Result<i32> {
    finish((|| {
        let request = Request::Desk {
            device: device.into(),
            action: action.into(),
            minutes,
        };
        match ask(request)? {
            Response::Message(text) => {
                println!("{device}: {text}");
                Ok(0)
            }
            _ => Err(unexpected()),
        }
    })())
}

/// `ferry crashes <устройство> [программа] [--last]`: падения на устройстве — в stdout.
pub fn crashes(device: &str, process: &str, last: bool) -> io::Result<i32> {
    finish((|| {
        let request = Request::Crashes {
            device: device.into(),
            process: process.into(),
            last,
        };
        match ask(request)? {
            Response::Message(text) => {
                print!("{text}");
                Ok(0)
            }
            _ => Err(unexpected()),
        }
    })())
}

/// `ferry forget <устройство>`: забыть связанное — только на этом устройстве.
pub fn forget(device: &str) -> io::Result<i32> {
    finish((|| {
        ask(Request::Forget {
            device: device.into(),
        })?;
        println!("{device}: забыто на этом устройстве");
        Ok(0)
    })())
}

/// `ferry run <цели> [--desk] [--no-sync] -- <команда>`: снимок проекта на цели, затем команда
/// в том же подкаталоге проекта (или в доме, если мы не в репозитории).
#[derive(Default)]
pub struct OutputOptions<'a> {
    pub grouped: bool,
    pub device: Option<&'a str>,
}
pub fn run_with_output(
    spec: &str,
    desk: bool,
    sync: bool,
    home: bool,
    cmd: &str,
    shell: Shell,
    options: OutputOptions<'_>,
) -> io::Result<i32> {
    let project = match project(sync, home) {
        Ok(project) => project,
        Err(fail) => return finish(Err(fail)),
    };
    let names = match filtered_targets(spec, options.device) {
        Ok(names) => names,
        Err(e) => return finish(Err(e)),
    };
    let job = |device: &str, printer: &Printer| {
        if let Some(project) = &project {
            sync_one(device, project, printer)?;
        }
        let request = Request::Run {
            device: device.into(),
            cmd: cmd.into(),
            dir: dir(&project),
            desk,
        };
        let request = if shell == Shell::Default {
            request
        } else {
            Request::RunShell {
                device: device.into(),
                cmd: cmd.into(),
                dir: dir(&project),
                desk,
                shell,
            }
        };
        stream(request, printer)
    };
    if options.grouped {
        grouped::each(&names, job)
    } else {
        each_names(&names, job)
    }
}

/// `ferry sync <цели>`.
pub fn sync(spec: &str) -> io::Result<i32> {
    let project = match project(true, false) {
        Ok(Some(project)) => project,
        Ok(None) => {
            return finish(Err(Fail::new(
                "здесь нет git-репозитория — синхронизировать нечего",
            )));
        }
        Err(fail) => return finish(Err(fail)),
    };
    each(spec, |device, printer| {
        sync_one(device, &project, printer).map(|()| 0)
    })
}

/// `ferry start <цели> <имя> [--no-sync] -- <команда>`: фоновая задача там же, где шла бы `run`.
pub fn start(
    spec: &str,
    name: &str,
    sync: bool,
    home: bool,
    cmd: &str,
    shell: Shell,
) -> io::Result<i32> {
    let project = match project(sync, home) {
        Ok(project) => project,
        Err(fail) => return finish(Err(fail)),
    };
    each(spec, |device, printer| {
        if let Some(project) = &project {
            sync_one(device, project, printer)?;
        }
        let request = Request::Start {
            device: device.into(),
            name: name.into(),
            cmd: cmd.into(),
            dir: dir(&project),
        };
        let request = if shell == Shell::Default {
            request
        } else {
            Request::StartShell {
                device: device.into(),
                name: name.into(),
                cmd: cmd.into(),
                dir: dir(&project),
                shell,
            }
        };
        said(ask(request)?, printer)
    })
}

/// `ferry jobs <цели>`.
/// `--json` — строка JSON на устройство, без подписи `[устройство]`: её разбирают построчно.
pub fn jobs(spec: &str, json: bool) -> io::Result<i32> {
    each(spec, |device, printer| {
        let Response::Jobs(jobs) = ask(Request::Jobs {
            device: device.into(),
        })?
        else {
            return Err(unexpected());
        };
        if json {
            println!("{}", serde_json::json!({ "device": device, "jobs": jobs }));
            return Ok(0);
        }
        if jobs.is_empty() {
            printer.line("ferry: задач нет", true);
        }
        for job in jobs {
            let cmd: String = job.cmd.chars().take(70).collect();
            printer.line(
                &format!(
                    "{:<14} {:<22} {:<12} {cmd}",
                    job.name,
                    job.label(),
                    job.ago()
                ),
                false,
            );
        }
        Ok(0)
    })
}

/// `ferry log <устройство> <имя> [-f]`: с `-f` — следить до конца задачи и выйти с её кодом.
pub fn log(device: &str, name: &str, follow: bool) -> io::Result<i32> {
    let request = Request::Log {
        device: device.into(),
        name: name.into(),
        follow,
    };
    finish(stream(request, &Printer::new(None)))
}

/// Машиночитаемый результат: всегда JSON, код процесса — код завершившейся задачи.
pub fn job_result(
    device: Option<&str>,
    name: &str,
    cursor: Option<&str>,
    max_bytes: u32,
    wait: u32,
) -> io::Result<i32> {
    finish((|| {
        let Response::JobResult(result) = ask(Request::JobResult {
            device: device.map(String::from),
            name: name.into(),
            cursor: cursor.map(String::from),
            max_bytes,
            wait,
        })?
        else {
            return Err(unexpected());
        };
        let code = result.command_code();
        let mut value = serde_json::to_value(result).map_err(|e| Fail::new(e.to_string()))?;
        value["device"] = device.unwrap_or("local").into();
        println!("{value}");
        Ok(code)
    })())
}

/// `ferry doctor <цели>`: готово ли устройство собирать этот проект.
pub fn doctor(spec: &str) -> io::Result<i32> {
    let repo = match sync::repo() {
        Ok(repo) => repo,
        Err(e) => return finish(Err(e.into())),
    };
    each(spec, |device, printer| {
        let repo = repo.as_ref().map(|r| (r.root.as_path(), r.name.as_str()));
        let checks = ferry_core::doctor::checks(device, repo)?;
        let script = ferry_core::doctor::script(&checks);
        let request = Request::Run {
            device: device.into(),
            cmd: script,
            dir: Dir::Home,
            desk: false,
        };
        let (output, _) = capture(request)?;
        let (lines, healthy) = ferry_core::doctor::report(&checks, &output);
        for line in lines {
            printer.line(&line, false);
        }
        Ok(if healthy { 0 } else { 1 })
    })
}

/// stdout команды целиком и её код — для разбора здесь.
fn capture(request: Request) -> Result<(String, i32), Fail> {
    let mut out = Vec::new();
    for response in control::open(request)? {
        match response? {
            Response::Out(data) => out.extend(data),
            Response::Err(_) => {}
            Response::Exit(code) => return Ok((String::from_utf8_lossy(&out).into_owned(), code)),
            Response::Error(fail) => return Err(fail),
            _ => return Err(unexpected()),
        }
    }
    Err(Fail::new("приложение оборвало ответ"))
}

/// `ferry kill <цели> <имя>`.
pub fn kill(spec: &str, name: &str) -> io::Result<i32> {
    each(spec, |device, printer| {
        said(
            ask(Request::Kill {
                device: device.into(),
                name: name.into(),
            })?,
            printer,
        )
    })
}

/// `ferry shot <цели> [файл.png]`: путь к снимку — в stdout. Без файла (и на нескольких целях) —
/// `~/.ferry/shots/<устройство>.png`.
/// Что снять: весь экран, окно (процесс или часть заголовка) или область `x,y,ширина,высота`.
pub enum Area<'a> {
    Screen,
    Window(&'a str),
    Region([i32; 4]),
}

pub fn shot(spec: &str, file: Option<&str>, area: Area) -> io::Result<i32> {
    each(spec, |device, printer| {
        let path = match file {
            Some(file) => std::path::absolute(file)?,
            None => ferry_core::ferry_home()?
                .join("shots")
                .join(format!("{device}.png")),
        };
        let region = match area {
            Area::Screen => None,
            Area::Region(region) => Some(region),
            // Место окна знает устройство: ответ — `x y ширина высота`.
            Area::Window(name) => {
                let Response::Message(place) = ask(Request::Input {
                    device: device.into(),
                    args: vec!["window".into(), name.into()],
                })?
                else {
                    return Err(unexpected());
                };
                let numbers: Vec<i32> = place
                    .split_whitespace()
                    .filter_map(|n| n.parse().ok())
                    .collect();
                let region: [i32; 4] = numbers.try_into().map_err(|_| unexpected())?;
                Some(region)
            }
        };
        match (&area, region) {
            (Area::Screen, _) | (_, None) => ask(Request::Shot {
                device: device.into(),
                file: path.clone(),
            })?,
            // Окно — само окно, даже закрытое другими; место нужно только для координат.
            (Area::Window(name), Some(_)) => ask(Request::ShotWindow {
                device: device.into(),
                file: path.clone(),
                name: (*name).into(),
            })?,
            (Area::Region(_), Some(area)) => ask(Request::ShotArea {
                device: device.into(),
                file: path.clone(),
                area,
            })?,
        };
        printer.line(&path.display().to_string(), false);
        // Клик и ввод ждут координаты всего экрана — к точке на снимке прибавить начало области.
        if let Some([x, y, width, height]) = region {
            printer.line(
                &format!(
                    "ferry: область {x},{y} {width}×{height} — для click прибавь {x},{y} к точке на снимке"
                ),
                true,
            );
        }
        Ok(0)
    })
}

/// Чего ждать: окна (процесс или часть заголовка), элемента или текста на экране в окне с
/// фокусом, открытого порта или текста в логе задачи.
#[derive(Clone, Copy)]
pub enum Wait<'a> {
    Window(&'a str),
    Name(&'a str),
    Text(&'a str),
    Port(u16),
    Log { job: &'a str, text: &'a str },
}

/// `ferry wait`: опрашивать устройство, пока окно, элемент или порт не появится (или не исчезнет
/// с `gone`), либо читать лог задачи, пока в нём не появится текст. Появилось — место или строку
/// лога на stdout, код 0; не дождался — код 1.
pub fn wait(device: &str, what: Wait, gone: bool, timeout: Duration) -> io::Result<i32> {
    let input = |args: [&str; 2]| Request::Input {
        device: device.into(),
        args: args.map(String::from).to_vec(),
    };
    // Глагол — по роду: окно появилось, элемент появился, порт открылся.
    let (label, [appeared, vanished]) = match what {
        Wait::Log { job, text } => return wait_log(device, job, text, timeout),
        Wait::Window(name) => (format!("окно «{name}»"), ["появилось", "исчезло"]),
        Wait::Name(name) => (format!("элемент «{name}»"), ["появился", "исчез"]),
        Wait::Text(text) => (format!("текст «{text}»"), ["появился", "исчез"]),
        Wait::Port(port) => (format!("порт {port}"), ["открылся", "закрылся"]),
    };
    let request = || match what {
        Wait::Window(name) => input(["probe-window", name]),
        Wait::Name(name) => input(["probe", name]),
        Wait::Text(text) => input(["probe-text", text]),
        Wait::Port(port) => Request::PortOpen {
            device: device.into(),
            port,
        },
        Wait::Log { .. } => unreachable!("лог читается потоком"),
    };
    let verb = if gone { vanished } else { appeared };
    let start = std::time::Instant::now();
    loop {
        // Есть ли сейчас и где: у окна и элемента — `found x y …`; несколько элементов — тоже
        // «есть».
        let answer = ask(request()).and_then(|response| match response {
            Response::Message(text) => {
                let text = text.trim();
                Ok((
                    text != "none",
                    text.strip_prefix("found ").map(String::from),
                ))
            }
            Response::Flag(open) => Ok((open, None)),
            _ => Err(unexpected()),
        });
        let (present, place) = match answer {
            Ok(answer) => answer,
            Err(fail) => return finish(Err(fail)),
        };
        if present != gone {
            let took = start.elapsed().as_secs_f64();
            if let Some(place) = place {
                println!("{place}");
            }
            eprintln!("ferry: {label} {verb} за {took:.1} с");
            return Ok(0);
        }
        if start.elapsed() >= timeout {
            eprintln!("ferry: {label} не {verb} за {} с", timeout.as_secs());
            return Ok(1);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Чем кончилось чтение лога: нашлась строка или задача вышла раньше.
enum Seen {
    Line(String),
    Exited(i32),
}

/// `ferry wait --log`: лог задачи потоком, как `ferry log -f`, пока в нём не появится `text`.
/// Он может уже быть в логе — тогда сразу.
fn wait_log(device: &str, job: &str, text: &str, timeout: Duration) -> io::Result<i32> {
    let start = std::time::Instant::now();
    let request = Request::Log {
        device: device.into(),
        name: job.into(),
        follow: true,
    };
    let mut matcher = LogMatch::new(text);
    let (tx, rx) = std::sync::mpsc::channel();
    // Поток ответа блокирующий — ждём его в своём потоке, чтобы уйти по таймауту.
    std::thread::spawn(move || {
        let seen = (|| {
            for response in control::open(request)? {
                match response? {
                    Response::Out(data) | Response::Err(data) => {
                        if let Some(line) = matcher.feed(&data) {
                            return Ok(Seen::Line(line));
                        }
                    }
                    Response::Exit(code) => return Ok(Seen::Exited(code)),
                    Response::Error(fail) => return Err(fail),
                    _ => return Err(unexpected()),
                }
            }
            Err(Fail::new("приложение оборвало ответ"))
        })();
        let _ = tx.send(seen);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(Seen::Line(line))) => {
            println!("{line}");
            let took = start.elapsed().as_secs_f64();
            eprintln!("ferry: «{text}» в логе задачи {job} — за {took:.1} с");
            Ok(0)
        }
        Ok(Ok(Seen::Exited(code))) => {
            eprintln!("ferry: задача {job} завершилась (код {code}), а «{text}» в её логе нет");
            Ok(1)
        }
        Ok(Err(fail)) => finish(Err(fail)),
        Err(_) => {
            eprintln!(
                "ferry: «{text}» не появилось в логе задачи {job} за {} с",
                timeout.as_secs()
            );
            Ok(1)
        }
    }
}

/// Поиск текста в логе, который приходит кусками. Строка копится до перевода строки — так
/// найдётся и текст, разрезанный между кусками; цветовые коды терминала и `\r` не мешают.
/// Нашёлся — вся строка (или её начало, если строка ещё не кончилась: приглашение без `\n`).
struct LogMatch {
    want: String,
    line: Vec<u8>,
}

impl LogMatch {
    fn new(want: &str) -> LogMatch {
        LogMatch {
            want: want.into(),
            line: Vec::new(),
        }
    }

    fn feed(&mut self, data: &[u8]) -> Option<String> {
        for &byte in data {
            if byte == b'\n' {
                if let Some(line) = self.check() {
                    return Some(line);
                }
                self.line.clear();
            } else {
                self.line.push(byte);
            }
        }
        // Строка без конца и без нужного текста — не копить её бесконечно.
        if self.line.len() > 1 << 20 {
            self.line.drain(..self.line.len() - (1 << 16));
        }
        self.check()
    }

    fn check(&self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.line);
        let plain = without_colors(text.trim_end_matches('\r'));
        plain.contains(&self.want).then_some(plain)
    }
}

/// Строка без управляющих последовательностей терминала (`ESC [ … буква`).
fn without_colors(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Окна и ввод: `windows`, `click`, `move`, `scroll`, `type`, `key`.
pub fn input(device: &str, args: Vec<String>) -> io::Result<i32> {
    finish(
        ask(Request::Input {
            device: device.into(),
            args,
        })
        .and_then(|response| match response {
            Response::Message(text) => {
                print!("{text}");
                Ok(0)
            }
            _ => Err(unexpected()),
        }),
    )
}

/// `ferry rec <устройство> <секунды> [кадр/с]`: кадры в `~/.ferry/recs/…`, в stdout — лист-обзор.
pub fn rec(device: &str, seconds: &str, fps: &str) -> io::Result<i32> {
    let ok = |s: &str, max: f64| s.parse::<f64>().is_ok_and(|v| v > 0.0 && v <= max);
    if !ok(seconds, 300.0) || !ok(fps, 15.0) {
        return finish(Err(Fail::new("секунды — до 300, кадров в секунду — до 15")));
    }
    finish((|| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let dest = ferry_core::ferry_home()?
            .join("recs")
            .join(format!("{device}-{now}"));
        let request = Request::Rec {
            device: device.into(),
            seconds: seconds.into(),
            fps: fps.into(),
            dest: dest.clone(),
        };
        let Response::Files(files) = ask(request)? else {
            return Err(unexpected());
        };
        let frames = files.iter().filter(|f| f.starts_with("frame-")).count();
        // ffmpeg необязателен: есть — склеим и видео.
        let video = dest.join("video.mp4");
        let encoded = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-framerate", fps, "-i"])
            .arg(dest.join("frame-%04d.png"))
            .args([
                "-pix_fmt",
                "yuv420p",
                "-vf",
                "scale=trunc(iw/2)*2:trunc(ih/2)*2",
            ])
            .arg(&video)
            .stdin(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        let video = if encoded {
            format!(", видео: {}", video.display())
        } else {
            String::new()
        };
        eprintln!("ferry: {frames} кадров в {}{video}", dest.display());
        // Лист-обзор собирает Windows; с мака — только кадры.
        let sheet = dest.join("sheet.png");
        println!("{}", if sheet.exists() { &sheet } else { &dest }.display());
        Ok(0)
    })())
}

/// `ferry info <цели>`: свойства и предупреждения.
pub fn info(spec: &str, json: bool) -> io::Result<i32> {
    each(spec, |device, printer| {
        let Response::Info(pairs) = ask(Request::Info {
            device: device.into(),
        })?
        else {
            return Err(unexpected());
        };
        if json {
            let info: serde_json::Map<_, _> = pairs
                .iter()
                .map(|(k, v)| (k.clone(), v.clone().into()))
                .collect();
            let warnings = ferry_core::info::warnings(&pairs);
            println!(
                "{}",
                serde_json::json!({ "device": device, "info": info, "warnings": warnings })
            );
            return Ok(0);
        }
        let shown = ferry_core::info::describe(&pairs);
        let width = shown
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0);
        for (label, value) in shown {
            printer.line(&format!("  {label:<width$}  {value}"), false);
        }
        for warning in ferry_core::info::warnings(&pairs) {
            printer.line(&format!("  ⚠ {warning}"), false);
        }
        Ok(0)
    })
}

/// `ferry power <цели> reboot|sleep|shutdown|cancel`.
pub fn power(spec: &str, action: &str) -> io::Result<i32> {
    each(spec, |device, printer| {
        said(
            ask(Request::Power {
                device: device.into(),
                action: action.into(),
            })?,
            printer,
        )
    })
}

/// `ferry update [<цели>]`: обновить ferry из релиза — на устройствах или на этой машине. Они
/// перезапустятся и снова выйдут на связь.
pub fn update(spec: Option<&str>) -> io::Result<i32> {
    let one =
        |device: Option<String>, printer: &Printer| said(ask(Request::Update { device })?, printer);
    match spec {
        None => finish(one(None, &Printer::new(None))),
        // Обновлять — и пульты: команды они не принимают, а обновление — да.
        Some(spec) => each_of(spec, true, |device, printer| {
            one(Some(device.into()), printer)
        }),
    }
}

/// Локальный пакет: передача, проверка подписи на устройстве, установка и новый запуск.
pub fn update_file(spec: Option<&str>, file: &str) -> io::Result<i32> {
    let file = std::fs::canonicalize(file)?;
    let package = match ferry_core::update::Package::read(&file) {
        Ok(p) => p,
        Err(fail) => return finish(Err(fail)),
    };
    let one = |device: Option<String>, printer: &Printer| -> Result<i32, Fail> {
        let status = || match ask(Request::UpdateStatus {
            device: device.clone(),
        })? {
            Response::UpdateStatus(status) => Ok(status),
            _ => Err(unexpected()),
        };
        let before = status()?;
        package.check_target(&before)?;
        printer.line(
            &format!(
                "передаю подписанный пакет ferry {} ({} МиБ)…",
                package.version,
                std::fs::metadata(&file)?.len().div_ceil(1024 * 1024)
            ),
            false,
        );
        said(
            ask(Request::UpdateFile {
                device: device.clone(),
                file: file.clone(),
                signature: package.signature.clone(),
            })?,
            printer,
        )?;
        printer.line("жду нового запуска устройства (до 180 с)…", false);
        let started = std::time::Instant::now();
        let mut last = String::new();
        while started.elapsed() < Duration::from_secs(180) {
            std::thread::sleep(Duration::from_secs(1));
            match status() {
                Ok(now) if now.restarted(&before, &package.version) => {
                    printer.line(
                        &format!(
                            "устройство перезапустилось и на связи: ferry {}",
                            now.version
                        ),
                        false,
                    );
                    return Ok(0);
                }
                Ok(now) => {
                    if let Some(error) = now.error {
                        return Err(Fail::new(error));
                    }
                    last = format!(
                        "ferry {}, {}",
                        now.version,
                        if now.boot == before.boot {
                            "ещё прежний процесс"
                        } else {
                            "версия не совпадает с пакетом"
                        }
                    );
                }
                Err(fail) if fail.code == 75 => return Err(fail),
                Err(fail) => last = fail.message,
            }
        }
        Err(Fail::new(format!(
            "новый запуск ferry {} не подтверждён за 180 с: {last}. Проверь ferry logs; повторная установка не запущена",
            package.version
        )))
    };
    match spec {
        None => finish(one(None, &Printer::new(None))),
        Some(spec) => each_of(spec, true, |device, printer| {
            one(Some(device.into()), printer)
        }),
    }
}

/// `ferry logs [<цели>] [-n <строк>]`: хвост лога приложения ferry — на устройствах или здесь.
pub fn logs(spec: Option<&str>, lines: usize) -> io::Result<i32> {
    let one =
        |device: Option<String>, printer: &Printer| match ask(Request::Logs { device, lines })? {
            Response::Message(text) => {
                printer.out(format!("{text}\n").as_bytes());
                Ok(0)
            }
            _ => Err(unexpected()),
        };
    match spec {
        None => finish(one(None, &Printer::new(None))),
        Some(spec) => each_of(spec, true, |device, printer| {
            one(Some(device.into()), printer)
        }),
    }
}

/// `ferry stop <цели>`: снять всё — команды и задачи.
pub fn stop(spec: &str) -> io::Result<i32> {
    each(spec, |device, printer| {
        said(
            ask(Request::Stop {
                device: device.into(),
            })?,
            printer,
        )
    })
}

/// Откуда считаются пути: корень репозитория (или текущий каталог вне его), куда на устройстве
/// и где мы внутри репозитория.
fn here() -> Result<(PathBuf, Dir, String), Fail> {
    Ok(match sync::repo()? {
        Some(repo) => (
            repo.root.canonicalize()?,
            Dir::Project {
                name: repo.name,
                prefix: String::new(),
            },
            repo.prefix,
        ),
        None => (
            std::env::current_dir()?.canonicalize()?,
            Dir::Home,
            String::new(),
        ),
    })
}

/// `ferry put <цели> <файл>...`: файлы и каталоги отсюда — на то же место в проекте устройства.
pub fn put(spec: &str, paths: &[&str]) -> io::Result<i32> {
    let prepared = (|| {
        let (base, dir, _) = here()?;
        let mut rels = Vec::new();
        for path in paths {
            let full =
                std::fs::canonicalize(path).map_err(|e| Fail::new(format!("{path}: {e}")))?;
            let rel = full
                .strip_prefix(&base)
                .map_err(|_| Fail::new(format!("{path}: вне {}", base.display())))?;
            if rel.as_os_str().is_empty() {
                return Err(Fail::new("весь проект целиком — это sync; назови файлы"));
            }
            rels.push(rel.to_path_buf());
        }
        Ok((base, dir, rels))
    })();
    let (base, dir, rels) = match prepared {
        Ok(prepared) => prepared,
        Err(fail) => return finish(Err(fail)),
    };
    each(spec, |device, printer| {
        let request = Request::Put {
            device: device.into(),
            base: base.clone(),
            rels: rels.clone(),
            dir: dir.clone(),
        };
        ask(request)?;
        let names: Vec<String> = rels.iter().map(|r| r.display().to_string()).collect();
        printer.line(&format!("ferry: {} → {device}", names.join(", ")), true);
        Ok(0)
    })
}

/// `ferry get <устройство> <путь> [куда]`: путь — от того же места в проекте, `~/…` — от дома
/// устройства, или абсолютный.
pub fn get(device: &str, path: &str, dest: Option<&str>) -> io::Result<i32> {
    finish((|| {
        let (_, dir, prefix) = here()?;
        let path = match &dir {
            Dir::Project { .. } if !path.starts_with("~/") && !Path::new(path).is_absolute() => {
                format!("{prefix}{path}")
            }
            _ => path.to_string(),
        };
        let dest = std::path::absolute(dest.unwrap_or("."))?;
        let request = Request::Get {
            device: device.into(),
            path: path.clone(),
            dir,
            dest: dest.clone(),
        };
        let Response::Files(files) = ask(request)? else {
            return Err(unexpected());
        };
        for file in files {
            eprintln!("ferry: {device}:{path} → {}", dest.join(file).display());
        }
        Ok(0)
    })())
}

/// `ferry pull <устройство>`: правки, сделанные на устройстве, — сюда.
pub fn pull(device: &str) -> io::Result<i32> {
    finish((|| {
        let repo = sync::repo()?.ok_or_else(|| Fail::new("здесь нет git-репозитория"))?;
        let request = Request::Pull {
            device: device.into(),
            root: repo.root,
            project: repo.name,
        };
        said(ask(request)?, &Printer::new(None))
    })())
}

/// `ferry tunnel [<устройство> <порт>] [--close]`: без аргументов — открытые туннели.
pub fn tunnel(target: Option<(&str, &str)>, close: bool) -> io::Result<i32> {
    finish((|| {
        let Some((device, port)) = target else {
            let Response::Tunnels(list) = ask(Request::Tunnels)? else {
                return Err(unexpected());
            };
            if list.is_empty() {
                eprintln!("ferry: туннелей нет; открыть — ferry tunnel <устройство> <порт>");
            }
            for (device, port) in list {
                println!("127.0.0.1:{port} → {device}:{port}");
            }
            return Ok(0);
        };
        let port: u16 = port
            .parse()
            .map_err(|_| Fail::new(format!("порт — число, а не {port:?}")))?;
        said(
            ask(Request::Tunnel {
                device: device.into(),
                port,
                close,
            })?,
            &Printer::new(None),
        )
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_match_finds_text_across_chunks() {
        let mut log = LogMatch::new("ready on");
        assert_eq!(log.feed(b"building...\nserver rea"), None);
        assert_eq!(
            log.feed(b"dy on :4242\nmore\n").as_deref(),
            Some("server ready on :4242"),
            "текст разрезан между кусками"
        );

        // Windows: \r\n; цвет терминала внутри текста; строка без перевода в конце.
        let mut log = LogMatch::new("Local: http");
        assert_eq!(
            log.feed(b"  \x1b[32mLocal\x1b[39m: http://localhost:5173/\r\n")
                .as_deref(),
            Some("  Local: http://localhost:5173/")
        );
        let mut log = LogMatch::new("password:");
        assert_eq!(
            log.feed(b"enter password:").as_deref(),
            Some("enter password:")
        );

        let mut log = LogMatch::new("ready");
        assert_eq!(log.feed(b"not yet\nstill not\n"), None);
        assert_eq!(
            log.feed(&vec![b'x'; 3 << 20]),
            None,
            "длинная строка не копится вся"
        );
        assert!(log.line.len() <= 1 << 20);
        assert_eq!(
            log.feed(b" ready\n")
                .as_deref()
                .map(|l| l.ends_with(" ready")),
            Some(true)
        );
    }
    use ferry_core::device::Status;
    use ferry_core::mode::Mode;

    #[test]
    fn table_lines_up_and_explains_offline() {
        let status = |name: &str, os: &str| Status {
            name: name.into(),
            role: Role::Node,
            mode: Mode::Active,
            os: os.into(),
            version: "0.1.0".into(),
        };
        let snap = Snapshot {
            id: "ff".into(),
            me: Status {
                role: Role::Both,
                ..status("macbook", "macos")
            },
            peers: vec![
                PeerView {
                    id: "c6".into(),
                    name: "win-10".into(),
                    online: true,
                    status: Some(status("win-10", "windows")),
                    direct: Some(true),
                    rtt_ms: Some(6),
                },
                PeerView {
                    id: "c7".into(),
                    name: "win-11".into(),
                    online: false,
                    status: None,
                    direct: None,
                    rtt_ms: None,
                },
            ],
            lan: Vec::new(),
            pairing: None,
            activity: Vec::new(),
        };
        assert_eq!(
            table(&snap),
            "  macbook   это устройство   macOS     узел и пульт   работает\n\
             \x20 win-10    напрямую, 6 мс   Windows   узел           работает\n\
             \x20 win-11    не на связи\n"
        );
    }

    #[test]
    fn prefixed_output_comes_out_in_whole_lines() {
        let printer = Printer::new(Some("win-10"));
        printer.out(b"half");
        assert_eq!(
            printer.out.lock().unwrap().as_slice(),
            b"half",
            "неполная строка ждёт"
        );
        printer.out(b" line\nnext");
        assert_eq!(printer.out.lock().unwrap().as_slice(), b"next");
        printer.flush();
        assert!(printer.out.lock().unwrap().is_empty());
    }
}
