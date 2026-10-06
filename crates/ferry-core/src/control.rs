//! Локальное управление приложением ferry: CLI → приложение на этой же машине. Приложение
//! слушает 127.0.0.1 и пишет порт и токен в `~/.ferry/app` — как демон рабочего стола.
//! Запрос и ответ — по строке JSON.

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::device::{PairingInfo, PeerView, Role, Snapshot};

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    State,
    OpenPairing,
    Pair {
        code: String,
    },
    /// Роль этого устройства (`device` пуст) или связанного — по имени или началу ключа.
    SetRole {
        device: Option<String>,
        role: Role,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Response {
    State(Snapshot),
    Pairing(PairingInfo),
    Peer(PeerView),
    Done,
    Error(String),
}

/// Строка запроса: токен и сам запрос.
#[derive(Serialize, Deserialize)]
pub struct Envelope {
    pub token: String,
    pub request: Request,
}

/// Отправить запрос приложению на этой машине.
pub fn call(request: Request) -> io::Result<Response> {
    call_in(&crate::ferry_home()?, request)
}

pub fn call_in(dir: &Path, request: Request) -> io::Result<Response> {
    let state = std::fs::read_to_string(dir.join("app")).map_err(|_| not_running())?;
    let (port, token) = state
        .trim()
        .split_once(' ')
        .and_then(|(port, token)| Some((port.parse::<u16>().ok()?, token.to_string())))
        .ok_or_else(|| io::Error::other(format!("испорчен {}", dir.join("app").display())))?;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|_| not_running())?;
    let line = serde_json::to_string(&Envelope { token, request }).map_err(io::Error::other)?;
    stream.write_all(format!("{line}\n").as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    serde_json::from_str(&answer).map_err(|e| io::Error::other(format!("ответ приложения: {e}")))
}

fn not_running() -> io::Error {
    io::Error::other("приложение ferry не запущено — открой его (оно живёт в трее)")
}
