//! Связывание по коду. Устройство, которое добавляют, показывает код (8 цифр, 10 минут,
//! 5 попыток); на другом его вводят. Код не уходит в сеть: SPAKE2 даёт обеим сторонам общий
//! ключ, только если код совпал, а подтверждение привязано к ключам обоих устройств. Чужое
//! устройство, выдающее себя за нужное, кода не узнает, а угадать может с одной попытки.
//!
//! В одной сети хватает кода — устройство находится само. Из другой сети вводят ссылку
//! `ferry:<ключ устройства>:<код>`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use hmac::{Hmac, Mac};
use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

pub const ALPN: &[u8] = b"ferry/pair/1";
const TTL: Duration = Duration::from_secs(10 * 60);
const ATTEMPTS: u32 = 5;

/// Открытое окно связывания на устройстве, которое добавляют.
#[derive(Debug)]
pub struct Window {
    pub code: String,
    expires: Instant,
    attempts: u32,
}

impl Window {
    pub fn open() -> Window {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).expect("системный генератор случайных чисел");
        let n = u32::from_le_bytes(bytes) % 100_000_000;
        Window {
            code: format!("{:04}-{:04}", n / 10_000, n % 10_000),
            expires: Instant::now() + TTL,
            attempts: ATTEMPTS,
        }
    }

    pub fn left(&self) -> Duration {
        self.expires.saturating_duration_since(Instant::now())
    }
}

/// Ссылка для связывания из другой сети.
pub fn ticket(id: EndpointId, code: &str) -> String {
    format!("ferry:{id}:{code}")
}

/// Код или ссылка → (устройство, если указано; код из 8 цифр).
pub fn parse(input: &str) -> Result<(Option<EndpointId>, String)> {
    let input = input.trim();
    let (id, code) = match input.strip_prefix("ferry:") {
        Some(rest) => {
            let (id, code) = rest
                .split_once(':')
                .context("ссылка — ferry:<ключ>:<код>")?;
            (Some(id.parse().context("ключ устройства в ссылке")?), code)
        }
        None => (None, input),
    };
    let digits: String = code.chars().filter(char::is_ascii_digit).collect();
    ensure!(
        digits.len() == 8
            && code
                .chars()
                .all(|c| c.is_ascii_digit() || c == '-' || c == ' '),
        "код — 8 цифр, например 1234-5678"
    );
    Ok((id, format!("{}-{}", &digits[..4], &digits[4..])))
}

#[derive(Serialize, Deserialize)]
struct Hello {
    name: String,
    spake: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Ok {
        name: String,
        spake: Vec<u8>,
        confirm: Vec<u8>,
    },
    Refused(String),
}

#[derive(Serialize, Deserialize)]
struct Confirm(Vec<u8>);

#[derive(Serialize, Deserialize)]
enum Done {
    Ok,
    Refused(String),
}

/// Сторона, которая вводит код. Возвращает имя нового устройства.
pub async fn initiate(conn: &Connection, me: EndpointId, name: &str, code: &str) -> Result<String> {
    let peer = conn.remote_id();
    let (spake, msg) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_bytes()),
        &Identity::new(me.as_bytes()),
        &Identity::new(peer.as_bytes()),
    );
    let (mut send, mut recv) = conn.open_bi().await?;
    write(
        &mut send,
        &Hello {
            name: name.into(),
            spake: msg,
        },
    )
    .await?;
    let (peer_name, their_msg, their_confirm) = match read(&mut recv).await? {
        Reply::Ok {
            name,
            spake,
            confirm,
        } => (name, spake, confirm),
        Reply::Refused(why) => bail!("{why}"),
    };
    let key = spake
        .finish(&their_msg)
        .map_err(|_| anyhow::anyhow!("код не подошёл"))?;
    // Сначала убедиться, что та сторона знает код, и только потом подтвердить своё.
    ensure!(
        confirmation(&key, b"responder", me, peer)
            .verify_slice(&their_confirm)
            .is_ok(),
        "код не подошёл"
    );
    let ours = confirmation(&key, b"initiator", me, peer)
        .finalize()
        .into_bytes()
        .to_vec();
    write(&mut send, &Confirm(ours)).await?;
    match read(&mut recv).await? {
        Done::Ok => Ok(peer_name),
        Done::Refused(why) => bail!("{why}"),
    }
}

/// Сторона, которая показала код. Возвращает имя устройства, которое его ввело.
pub async fn respond(
    conn: &Connection,
    me: EndpointId,
    name: &str,
    window: &Mutex<Option<Window>>,
) -> Result<String> {
    let peer = conn.remote_id();
    let (mut send, mut recv) = conn.accept_bi().await?;
    let hello: Hello = read(&mut recv).await?;
    // Попытка списывается до проверки: перебирать код нельзя, даже обрывая соединение.
    let code = {
        let mut window = window.lock().unwrap();
        match window.as_mut() {
            Some(w) if w.left() > Duration::ZERO && w.attempts > 0 => {
                w.attempts -= 1;
                Some(w.code.clone())
            }
            Some(_) => {
                *window = None;
                None
            }
            None => None,
        }
    };
    let Some(code) = code else {
        let why = "это устройство сейчас не ждёт связывания — открой «Связать» на нём ещё раз";
        last(conn, &mut send, &Reply::Refused(why.into())).await?;
        bail!("{why}");
    };
    let (spake, msg) = Spake2::<Ed25519Group>::start_b(
        &Password::new(code.as_bytes()),
        &Identity::new(peer.as_bytes()),
        &Identity::new(me.as_bytes()),
    );
    let key = spake
        .finish(&hello.spake)
        .map_err(|_| anyhow::anyhow!("код не подошёл"))?;
    let confirm = confirmation(&key, b"responder", peer, me)
        .finalize()
        .into_bytes()
        .to_vec();
    write(
        &mut send,
        &Reply::Ok {
            name: name.into(),
            spake: msg,
            confirm,
        },
    )
    .await?;
    let Confirm(theirs) = read(&mut recv).await?;
    if confirmation(&key, b"initiator", peer, me)
        .verify_slice(&theirs)
        .is_err()
    {
        last(conn, &mut send, &Done::Refused("код не подошёл".into())).await?;
        bail!("{} ввёл неверный код", hello.name);
    }
    *window.lock().unwrap() = None;
    last(conn, &mut send, &Done::Ok).await?;
    Ok(hello.name)
}

/// Подтверждение знания кода: HMAC общего ключа по роли и ключам обоих устройств.
fn confirmation(
    key: &[u8],
    role: &[u8],
    initiator: EndpointId,
    responder: EndpointId,
) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC принимает ключ любой длины");
    mac.update(b"ferry pair confirm ");
    mac.update(role);
    mac.update(initiator.as_bytes());
    mac.update(responder.as_bytes());
    mac
}

/// Последний ответ: дописать и дождаться, пока соединение закроет та сторона, — закрытие
/// отсюда оборвало бы ещё не доставленный ответ.
async fn last<T: Serialize>(conn: &Connection, send: &mut SendStream, value: &T) -> Result<()> {
    write(send, value).await?;
    send.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
    Ok(())
}

/// Кадр — длина (u32) и JSON. Больше 64 КБ при связывании не бывает.
async fn write<T: Serialize>(send: &mut SendStream, value: &T) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    send.write_all(&(body.len() as u32).to_be_bytes()).await?;
    send.write_all(&body).await?;
    Ok(())
}

async fn read<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    ensure!(len <= 64 * 1024, "слишком длинное сообщение связывания");
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_tickets_parse() {
        let code = Window::open().code;
        assert_eq!(code.len(), 9);
        assert_eq!(parse(&code).unwrap(), (None, code.clone()));
        assert_eq!(parse(" 12345678 ").unwrap().1, "1234-5678");
        assert!(parse("1234-567").is_err());
        assert!(parse("1234-56789").is_err());
        assert!(parse("abcd-efgh").is_err());

        let id = iroh::SecretKey::generate().public();
        assert_eq!(parse(&ticket(id, &code)).unwrap(), (Some(id), code));
        assert!(parse("ferry:not-a-key:1234-5678").is_err());
    }
}
