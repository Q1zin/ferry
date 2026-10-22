//! Локальное управление приложением ferry: CLI → приложение на этой же машине. Приложение
//! слушает 127.0.0.1 и пишет порт и токен в `~/.ferry/app` — как демон рабочего стола.
//! Запрос — строка JSON; ответ — строка JSON, а у `Run` и `Log` — поток строк: куски вывода и
//! в конце код выхода.
//!
//! Устройство в запросах — по имени или началу ключа. Файлы CLI и приложение передают путями:
//! они на одной машине.

use std::io::{self, BufRead, BufReader, Lines, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::device::{Dir, Fail, PairingInfo, PeerView, Role, Snapshot};
use crate::jobs::Job;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    State,
    ExecutionCancel {
        device: Option<String>,
        request_id: String,
    },
    Storage {
        device: Option<String>,
        request: crate::storage::Request,
    },
    Human {
        device: Option<String>,
        request: crate::human::Request,
    },
    DeskNote {
        device: String,
        minutes: u32,
        note: String,
    },
    Session {
        request: crate::session::Request,
    },
    OwnedTunnel {
        device: String,
        port: u16,
        close: bool,
        keep: bool,
    },
    AtomicStart {
        device: Option<String>,
        request: crate::jobs::execution::StartRequest,
        local_cwd: Option<PathBuf>,
    },
    ExecutionLookup {
        device: Option<String>,
        request_id: String,
    },
    WorkspaceSync {
        device: Option<String>,
        root: PathBuf,
        project: String,
        id: String,
        commit: String,
        tree: String,
    },
    WorkspacePatch {
        device: Option<String>,
        root: PathBuf,
        source: crate::jobs::execution::Source,
        apply: Option<String>,
    },
    ObserveV2 {
        device: Option<String>,
        request: crate::screen::observe::RequestV2,
        file: PathBuf,
    },
    ActV2 {
        device: Option<String>,
        request: Box<crate::screen::act::RequestV2>,
        file: PathBuf,
    },
    Act {
        device: Option<String>,
        request: crate::screen::act::Request,
        file: PathBuf,
    },
    Observe {
        device: Option<String>,
        request: crate::screen::observe::Request,
        file: PathBuf,
    },
    Evidence {
        device: Option<String>,
        query: crate::jobs::evidence::Query,
    },
    Capabilities {
        device: String,
        request: crate::capabilities::Request,
    },
    RunShell {
        device: String,
        cmd: String,
        dir: Dir,
        desk: bool,
        shell: crate::desk::Shell,
    },
    StartShell {
        device: String,
        name: String,
        cmd: String,
        dir: Dir,
        shell: crate::desk::Shell,
    },
    OpenPairing,
    Pair {
        code: String,
    },
    /// Роль этого устройства (`device` пуст) или связанного.
    SetRole {
        device: Option<String>,
        role: Role,
    },
    /// Снимок `commit` (дерево `tree`) репозитория `root` — в `~/ferry/<project>` устройства.
    Sync {
        device: String,
        root: PathBuf,
        project: String,
        commit: String,
        tree: String,
    },
    Run {
        device: String,
        cmd: String,
        dir: Dir,
        desk: bool,
    },
    Start {
        device: String,
        name: String,
        cmd: String,
        dir: Dir,
    },
    Jobs {
        device: String,
    },
    Log {
        device: String,
        name: String,
        follow: bool,
    },
    Kill {
        device: String,
        name: String,
    },
    /// Снимок экрана — в PNG-файл `file`.
    Shot {
        device: String,
        file: PathBuf,
    },
    Input {
        device: String,
        args: Vec<String>,
    },
    /// Запись экрана — кадры и лист-обзор в каталог `dest`.
    Rec {
        device: String,
        seconds: String,
        fps: String,
        dest: PathBuf,
    },
    Info {
        device: String,
    },
    Power {
        device: String,
        action: String,
    },
    Stop {
        device: String,
    },
    /// Файлы `rels` (от `base`) — в `dir` устройства.
    Put {
        device: String,
        base: PathBuf,
        rels: Vec<PathBuf>,
        dir: Dir,
    },
    /// Файл или каталог с устройства — в каталог `dest`.
    Get {
        device: String,
        path: String,
        dir: Dir,
        dest: PathBuf,
    },
    /// Правки с устройства — в рабочее дерево `root`.
    Pull {
        device: String,
        root: PathBuf,
        project: String,
    },
    Tunnel {
        device: String,
        port: u16,
        close: bool,
    },
    Tunnels,
    JobResult {
        device: Option<String>,
        name: String,
        cursor: Option<String>,
        max_bytes: u32,
        wait: u32,
    },
    /// Обновить ferry из релиза: на устройстве или (`device` пуст) на этой машине.
    Update {
        device: Option<String>,
    },
    UpdateFile {
        device: Option<String>,
        file: PathBuf,
        signature: String,
    },
    UpdateStatus {
        device: Option<String>,
    },
    /// Снимок области `[x, y, ширина, высота]` в пикселях снимка — в файл `file`.
    ShotArea {
        device: String,
        file: PathBuf,
        area: [i32; 4],
    },
    /// Снимок самого окна (процесс или часть заголовка) — в файл `file`.
    ShotWindow {
        device: String,
        file: PathBuf,
        name: String,
    },
    /// Хвост лога приложения: устройства или (`device` пуст) этой машины.
    Logs {
        device: Option<String>,
        lines: usize,
    },
    /// Забыть связанное устройство — только здесь, как кнопка «Забыть устройство».
    Forget {
        device: String,
    },
    /// Бронь рабочего стола устройства: показать, занять, снять.
    Desk {
        device: String,
        action: String,
        minutes: u32,
    },
    /// Падения программ на устройстве.
    Crashes {
        device: String,
        process: String,
        last: bool,
    },
    /// Слушает ли кто-нибудь `port` на устройстве.
    PortOpen {
        device: String,
        port: u16,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Response {
    State(Snapshot),
    Report(String),
    Capabilities(crate::capabilities::Capabilities),
    UpdateStatus(crate::update::Status),
    Pairing(PairingInfo),
    Peer(PeerView),
    Done,
    /// Ответ для человека: что сделано.
    Message(String),
    Error(Fail),
    /// Кусок stdout или stderr команды.
    Out(#[serde(with = "base64")] Vec<u8>),
    Err(#[serde(with = "base64")] Vec<u8>),
    /// Код выхода — последняя строка потока.
    Exit(i32),
    Jobs(Vec<Job>),
    JobResult(crate::jobs::JobResult),
    Info(Vec<(String, String)>),
    /// Что легло на диск.
    Files(Vec<String>),
    Tunnels(Vec<(String, u16)>),
    /// Да или нет: порт открыт.
    Flag(bool),
}

impl Response {
    /// Конец ответа: дальше строк не будет.
    pub fn last(&self) -> bool {
        !matches!(self, Response::Out(_) | Response::Err(_))
    }
}

mod base64 {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(d)?)
            .map_err(D::Error::custom)
    }
}

/// Строка запроса: токен и сам запрос.
#[derive(Serialize, Deserialize)]
pub struct Envelope {
    pub token: String,
    pub request: Request,
    /// Кто просит — по нему устройство бронирует рабочий стол: [`session`].
    #[serde(default)]
    pub session: String,
}

/// Кто вызвал CLI: `FERRY_AGENT`, иначе сессия Claude Code — ближайший предок `claude` (каждая
/// сессия — свой процесс, команды — его потомки), иначе пусто — человек в терминале.
pub fn session() -> String {
    static SESSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SESSION
        .get_or_init(|| match std::env::var("FERRY_AGENT") {
            Ok(name) if !name.is_empty() => name,
            _ => claude_ancestor().map_or_else(String::new, |pid| format!("claude:{pid}")),
        })
        .clone()
}

fn claude_ancestor() -> Option<u32> {
    ancestor_named(&process_table().ok()?, std::process::id(), "claude")
}

fn program_name(comm: &str) -> &str {
    let name = comm.rsplit(['/', '\\']).next().unwrap_or(comm);
    name.strip_suffix(".exe").unwrap_or(name)
}

/// Только автоматически определяемые сессии имеют проверяемый срок жизни.
pub fn session_pid(session: &str) -> Option<u32> {
    session
        .strip_prefix("claude:")?
        .parse()
        .ok()
        .filter(|pid| *pid > 1)
}

pub fn ended_sessions(sessions: &[String]) -> io::Result<Vec<String>> {
    let table = process_table()?;
    Ok(sessions
        .iter()
        .filter(|session| {
            if session.starts_with("agent:") {
                return crate::ferry_home()
                    .and_then(|root| crate::session::ended(&root, session))
                    .unwrap_or(false);
            }
            session_pid(session).is_some_and(|pid| {
                !table.iter().any(|(id, _, comm)| {
                    *id == pid && program_name(comm).eq_ignore_ascii_case("claude")
                })
            })
        })
        .cloned()
        .collect())
}

#[cfg(unix)]
fn process_table() -> io::Result<Vec<(u32, u32, String)>> {
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,comm="])
        .stderr(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other("не удалось прочитать процессы"));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let comm = fields.collect::<Vec<_>>().join(" ");
            Some((pid, ppid, comm))
        })
        .collect())
}

#[cfg(windows)]
fn process_table() -> io::Result<Vec<(u32, u32, String)>> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Entry {
        size: u32,
        usage: u32,
        pid: u32,
        heap: usize,
        module: u32,
        threads: u32,
        parent: u32,
        priority: i32,
        flags: u32,
        name: [u16; 260],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
        fn Process32FirstW(snapshot: *mut c_void, entry: *mut Entry) -> i32;
        fn Process32NextW(snapshot: *mut c_void, entry: *mut Entry) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(2, 0);
        if snapshot as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut entry: Entry = std::mem::zeroed();
        entry.size = std::mem::size_of::<Entry>() as u32;
        let mut table = Vec::new();
        let mut found = Process32FirstW(snapshot, &mut entry);
        while found != 0 {
            let end = entry
                .name
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.name.len());
            table.push((
                entry.pid,
                entry.parent,
                String::from_utf16_lossy(&entry.name[..end]),
            ));
            found = Process32NextW(snapshot, &mut entry);
        }
        let error = io::Error::last_os_error();
        CloseHandle(snapshot);
        // ERROR_NO_MORE_FILES: снимок прочитан полностью, остальные ошибки не означают смерть агента.
        if error.raw_os_error() != Some(18) {
            return Err(error);
        }
        Ok(table)
    }
}

pub fn descendant_pids(root: u32) -> io::Result<Vec<u32>> {
    let table = process_table()?;
    let mut ids = vec![root];
    for _ in 0..64 {
        let more: Vec<_> = table
            .iter()
            .filter(|(id, parent, _)| !ids.contains(id) && ids.contains(parent))
            .map(|(id, _, _)| *id)
            .collect();
        if more.is_empty() {
            break;
        }
        ids.extend(more);
    }
    Ok(ids)
}

/// Ближайший предок `pid` с программой `name` по таблице (pid, ppid, путь программы).
fn ancestor_named(table: &[(u32, u32, String)], pid: u32, name: &str) -> Option<u32> {
    let mut current = pid;
    for _ in 0..64 {
        let &(_, parent, _) = table.iter().find(|(p, _, _)| *p == current)?;
        let (_, _, comm) = table.iter().find(|(p, _, _)| *p == parent)?;
        if program_name(comm).eq_ignore_ascii_case(name) {
            return Some(parent);
        }
        if parent <= 1 {
            return None;
        }
        current = parent;
    }
    None
}

/// Отправить запрос приложению на этой машине и получить ответ.
pub fn call(request: Request) -> io::Result<Response> {
    call_in(&crate::ferry_home()?, request)
}

pub fn call_in(dir: &Path, request: Request) -> io::Result<Response> {
    open_in(dir, request)?
        .next()
        .unwrap_or_else(|| Err(io::Error::other("приложение не ответило")))
}

/// Отправить запрос и читать ответ построчно — для потоков `Run` и `Log`. Закрыл итератор —
/// соединение рвётся, и приложение останавливает команду на устройстве.
pub fn open(request: Request) -> io::Result<Answer> {
    open_in(&crate::ferry_home()?, request)
}

pub fn open_in(dir: &Path, request: Request) -> io::Result<Answer> {
    open_session(dir, request, &session(), None)
}
pub fn call_session(request: Request, session: &str) -> io::Result<Response> {
    let timeout = if matches!(
        request,
        Request::Sync { .. } | Request::WorkspaceSync { .. }
    ) {
        600
    } else {
        90
    };
    open_session(
        &crate::ferry_home()?,
        request,
        session,
        Some(std::time::Duration::from_secs(timeout)),
    )?
    .next()
    .unwrap_or_else(|| Err(io::Error::other("приложение не ответило")))
}
fn open_session(
    dir: &Path,
    request: Request,
    session: &str,
    timeout: Option<std::time::Duration>,
) -> io::Result<Answer> {
    let state = std::fs::read_to_string(dir.join("app")).map_err(|_| not_running())?;
    let (port, token) = state
        .trim()
        .split_once(' ')
        .and_then(|(port, token)| Some((port.parse::<u16>().ok()?, token.to_string())))
        .ok_or_else(|| io::Error::other(format!("испорчен {}", dir.join("app").display())))?;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|_| not_running())?;
    stream.set_read_timeout(timeout)?;
    if matches!(&request, Request::UpdateStatus { .. }) {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    }
    if let Request::JobResult { wait, .. } = &request {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(
            u64::from((*wait).min(60)) + 10,
        )))?;
    }
    if matches!(
        &request,
        Request::Info { .. }
            | Request::Crashes { .. }
            | Request::ActV2 { .. }
            | Request::ObserveV2 { .. }
            | Request::AtomicStart { .. }
            | Request::Act { .. }
            | Request::Observe { .. }
            | Request::Evidence { .. }
            | Request::Capabilities { .. }
    ) {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(90)))?;
    }
    let envelope = Envelope {
        token,
        request,
        session: session.into(),
    };
    let line = serde_json::to_string(&envelope).map_err(io::Error::other)?;
    stream.write_all(format!("{line}\n").as_bytes())?;
    Ok(Answer {
        lines: BufReader::new(stream).lines(),
        done: false,
    })
}

/// Ответ приложения построчно; кончается на первой «последней» строке.
pub struct Answer {
    lines: Lines<BufReader<TcpStream>>,
    done: bool,
}

impl Iterator for Answer {
    type Item = io::Result<Response>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let line = match self.lines.next() {
            Some(Ok(line)) => line,
            Some(Err(e)) => return Some(Err(e)),
            None => {
                self.done = true;
                return Some(Err(io::Error::other("приложение оборвало ответ")));
            }
        };
        let response: Response = match serde_json::from_str(&line) {
            Ok(response) => response,
            Err(e) => return Some(Err(io::Error::other(format!("ответ приложения: {e}")))),
        };
        self.done = response.last();
        Some(Ok(response))
    }
}

fn not_running() -> io::Error {
    io::Error::other("приложение ferry не запущено — открой его (оно живёт в трее)")
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_the_claude_session_above() {
        let table = vec![
            (1, 0, "/sbin/launchd".to_string()),
            (
                500,
                1,
                "/Applications/Code.app/Contents/MacOS/Code".to_string(),
            ),
            (
                600,
                500,
                "/Users/a1/.vscode/extensions/anthropic.claude-code/bin/claude".to_string(),
            ),
            (700, 600, "/bin/zsh".to_string()),
            (800, 700, "ferry".to_string()),
            (900, 500, "/bin/zsh".to_string()),
        ];
        assert_eq!(ancestor_named(&table, 800, "claude"), Some(600));
        // Терминал без Claude Code — нет сессии; чужого pid в таблице нет — тоже.
        assert_eq!(ancestor_named(&table, 900, "claude"), None);
        assert_eq!(ancestor_named(&table, 42, "claude"), None);
    }

    #[test]
    fn windows_sessions_follow_native_process_ancestry() {
        let table = vec![
            (100, 1, r"C:\Apps\claude.exe".into()),
            (200, 100, "powershell.exe".into()),
            (300, 200, "ferry.exe".into()),
            (400, 1, "claude.exe".into()),
            (500, 400, "ferry.exe".into()),
        ];
        assert_eq!(ancestor_named(&table, 300, "claude"), Some(100));
        assert_eq!(ancestor_named(&table, 500, "claude"), Some(400));
        assert_eq!(session_pid("claude:100"), Some(100));
        assert_eq!(session_pid("claude:0"), None);
        assert_eq!(session_pid("my-agent"), None);
    }

    #[test]
    fn reads_native_process_table_without_releasing_named_sessions() {
        let table = process_table().unwrap();
        assert!(table.iter().any(|(pid, _, _)| *pid == std::process::id()));
        let missing = format!("claude:{}", u32::MAX);
        assert_eq!(
            ended_sessions(&[missing.clone(), "manual".into()]).unwrap(),
            vec![missing]
        );
    }

    use super::*;

    #[test]
    fn output_bytes_survive_json() {
        let bytes = vec![0xff, b'\n', 0xd0, 0x9f, 0x00];
        let line = serde_json::to_string(&Response::Out(bytes.clone())).unwrap();
        assert_eq!(
            serde_json::from_str::<Response>(&line).unwrap(),
            Response::Out(bytes)
        );
        assert!(!Response::Out(vec![]).last() && Response::Exit(0).last());
    }
}
