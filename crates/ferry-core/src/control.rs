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
    /// Обновить ferry из релиза: на устройстве или (`device` пуст) на этой машине.
    Update {
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

/// Ближайший предок-процесс `claude`: одна таблица процессов `ps`, по ней — вверх от себя.
#[cfg(unix)]
fn claude_ancestor() -> Option<u32> {
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,comm="])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let table: Vec<(u32, u32, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let comm = fields.collect::<Vec<_>>().join(" ");
            Some((pid, ppid, comm))
        })
        .collect();
    ancestor_named(&table, std::process::id(), "claude")
}

#[cfg(not(unix))]
fn claude_ancestor() -> Option<u32> {
    None
}

/// Ближайший предок `pid` с программой `name` по таблице (pid, ppid, путь программы).
#[cfg_attr(not(unix), allow(dead_code))]
fn ancestor_named(table: &[(u32, u32, String)], pid: u32, name: &str) -> Option<u32> {
    let mut current = pid;
    for _ in 0..64 {
        let &(_, parent, _) = table.iter().find(|(p, _, _)| *p == current)?;
        let (_, _, comm) = table.iter().find(|(p, _, _)| *p == parent)?;
        if comm.rsplit('/').next() == Some(name) {
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
    let state = std::fs::read_to_string(dir.join("app")).map_err(|_| not_running())?;
    let (port, token) = state
        .trim()
        .split_once(' ')
        .and_then(|(port, token)| Some((port.parse::<u16>().ok()?, token.to_string())))
        .ok_or_else(|| io::Error::other(format!("испорчен {}", dir.join("app").display())))?;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|_| not_running())?;
    let session = session();
    let envelope = Envelope {
        token,
        request,
        session,
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
