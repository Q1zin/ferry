//! Устройства — через приложение ferry на этой машине: оно держит связь, CLI его спрашивает.
//!
//! Цели команд: имя устройства (или начало ключа), несколько через запятую, `all` — все на
//! связи, которые принимают команды, `os:windows` / `os:macos` — по системе. На нескольких
//! целях команда идёт параллельно, строки вывода подписаны `[устройство]`, код выхода — первый
//! ненулевой по порядку целей.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ferry_core::control::{self, Request, Response};
use ferry_core::device::{Dir, Fail, PeerView, Role, Snapshot};
use ferry_core::sync;

fn ask(request: Request) -> Result<Response, Fail> {
    match control::call(request)? {
        Response::Error(fail) => Err(fail),
        response => Ok(response),
    }
}

fn unexpected() -> Fail {
    Fail::new("приложение ответило не то — версии приложения и CLI разные?")
}

fn state() -> Result<Snapshot, Fail> {
    match ask(Request::State)? {
        Response::State(snap) => Ok(snap),
        _ => Err(unexpected()),
    }
}

/// Ошибка устройства — сообщением и её кодом (75 — отказ по режиму или роли).
fn finish(result: Result<i32, Fail>) -> io::Result<i32> {
    Ok(result.unwrap_or_else(|fail| {
        eprintln!("ferry: {fail}");
        fail.code
    }))
}

// --- Цели ------------------------------------------------------------------------------------

/// Имена устройств по спецификации целей.
fn targets(spec: &str) -> Result<Vec<String>, Fail> {
    let snap = state()?;
    let mut names = Vec::new();
    let mut skipped = Vec::new();
    let accepts = |p: &PeerView| p.status.as_ref().is_some_and(|s| s.role.accepts_commands());
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

/// Вывод одной цели: как есть или строками с подписью устройства.
struct Printer {
    prefix: Option<String>,
    out: Mutex<Vec<u8>>,
    err: Mutex<Vec<u8>>,
}

/// Строки разных устройств не перемешиваются внутри строки.
static LINES: Mutex<()> = Mutex::new(());

impl Printer {
    fn new(prefix: Option<&str>) -> Printer {
        Printer {
            prefix: prefix.map(|p| format!("[{p}] ")),
            out: Mutex::default(),
            err: Mutex::default(),
        }
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
    let names = match targets(spec) {
        Ok(names) => names,
        Err(fail) => return finish(Err(fail)),
    };
    if let [name] = names.as_slice() {
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
struct Project {
    repo: sync::Repo,
    snapshot: Option<(String, String)>,
}

fn project(snapshot: bool) -> Result<Option<Project>, Fail> {
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
fn dir(project: &Option<Project>) -> Dir {
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
            Response::Out(data) => printer.out(&data),
            Response::Err(data) => printer.err(&data),
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
pub fn list() -> io::Result<i32> {
    finish(state().map(|snap| {
        print!("{}", table(&snap));
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

/// `ferry run <цели> [--desk] [--no-sync] -- <команда>`: снимок проекта на цели, затем команда
/// в том же подкаталоге проекта (или в доме, если мы не в репозитории).
pub fn run(spec: &str, desk: bool, sync: bool, cmd: &str) -> io::Result<i32> {
    let project = match project(sync) {
        Ok(project) => project,
        Err(fail) => return finish(Err(fail)),
    };
    each(spec, |device, printer| {
        if let Some(project) = &project {
            sync_one(device, project, printer)?;
        }
        let request = Request::Run {
            device: device.into(),
            cmd: cmd.into(),
            dir: dir(&project),
            desk,
        };
        stream(request, printer)
    })
}

/// `ferry sync <цели>`.
pub fn sync(spec: &str) -> io::Result<i32> {
    let project = match project(true) {
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
pub fn start(spec: &str, name: &str, sync: bool, cmd: &str) -> io::Result<i32> {
    let project = match project(sync) {
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
        said(ask(request)?, printer)
    })
}

/// `ferry jobs <цели>`.
pub fn jobs(spec: &str) -> io::Result<i32> {
    each(spec, |device, printer| {
        let Response::Jobs(jobs) = ask(Request::Jobs {
            device: device.into(),
        })?
        else {
            return Err(unexpected());
        };
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
pub fn shot(spec: &str, file: Option<&str>) -> io::Result<i32> {
    each(spec, |device, printer| {
        let path = match file {
            Some(file) => std::path::absolute(file)?,
            None => ferry_core::ferry_home()?
                .join("shots")
                .join(format!("{device}.png")),
        };
        ask(Request::Shot {
            device: device.into(),
            file: path.clone(),
        })?;
        printer.line(&path.display().to_string(), false);
        Ok(0)
    })
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
        println!("{}", dest.join("sheet.png").display());
        Ok(0)
    })())
}

/// `ferry info <цели>`: свойства и предупреждения.
pub fn info(spec: &str) -> io::Result<i32> {
    each(spec, |device, printer| {
        let Response::Info(pairs) = ask(Request::Info {
            device: device.into(),
        })?
        else {
            return Err(unexpected());
        };
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
        Some(spec) => each(spec, |device, printer| one(Some(device.into()), printer)),
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
