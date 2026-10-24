//! Релей git через устройство, которое запустило команду. У устройства-узла нет ключей от
//! приватных репозиториев, а git на нём ходит в них по SSH — ключами того, кто запустил.
//!
//! Команды, которые прислало устройство X, получают `GIT_SSH_COMMAND='<этот exe> ssh'` и
//! `FERRY_RELAY='<порт> <токен>'`, `FERRY_TRUST` — доверенные хосты X. `ferry ssh` (старый клиент
//! релея, [`ferry_core::relay::ssh`]) стучится на `127.0.0.1:<порт>` — сюда, в приложение, а оно
//! гонит сессию по iroh к X. X пропускает только чтение к своим доверенным хостам и выполняет
//! её своим ssh: ключ X не покидает.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ferry_core::mode::Mode;
use ferry_core::{exit_code, relay};
use iroh::EndpointId;
use irpc::channel::mpsc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::proto::{DeviceClient, GitSsh, Relay, Status};

/// Самое длинное поле протокола, как у демона рабочего стола.
const MAX_FIELD: u32 = 1 << 20;

/// Локальный вход релея на этом устройстве: порт и токен на каждое устройство-заказчика.
pub(crate) struct Local {
    dir: PathBuf,
    port: u16,
    tokens: Mutex<HashMap<EndpointId, String>>,
}

impl Local {
    pub async fn bind(dir: &Path, peers: &[EndpointId]) -> std::io::Result<(Self, TcpListener)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let local = Self {
            dir: dir.join("relays"),
            port: listener.local_addr()?.port(),
            tokens: Mutex::default(),
        };
        std::fs::create_dir_all(&local.dir)?;
        // Новые порт и токены публикуются до запуска команд; старые задачи читают их заново.
        for &peer in peers {
            local.token(peer)?;
        }
        Ok((local, listener))
    }

    fn token(&self, from: EndpointId) -> std::io::Result<String> {
        let mut tokens = self.tokens.lock().unwrap();
        if let Some(token) = tokens.get(&from) {
            return Ok(token.clone());
        }
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        ferry_core::write_private(
            &self.dir.join(from.to_string()),
            &format!("{} {token}\n", self.port),
        )?;
        tokens.insert(from, token.clone());
        Ok(token)
    }

    /// Окружение команды, которую прислало `from`: git к его доверенным хостам `trust` — через
    /// его ssh. `shim` — бинарник с `ssh` (само приложение или `ferry`).
    pub fn env(
        &self,
        from: EndpointId,
        trust: &[String],
        shim: &Path,
    ) -> std::io::Result<Vec<(String, String)>> {
        if trust.is_empty() {
            return Ok(Vec::new());
        }
        let token = self.token(from)?;
        let port = self.port;
        // Прямые слэши: команду разбирает шелл узла (Git Bash на Windows).
        let shim = shim.to_string_lossy().replace('\\', "/");
        Ok([
            ("GIT_SSH_COMMAND", format!("'{shim}' ssh")),
            ("GIT_SSH_VARIANT", "ssh".into()),
            // cargo по умолчанию ходит в git через libgit2, мимо GIT_SSH_COMMAND.
            ("CARGO_NET_GIT_FETCH_WITH_CLI", "true".into()),
            ("FERRY_RELAY", format!("{port} {token}")),
            (
                "FERRY_RELAY_FILE",
                self.dir
                    .join(from.to_string())
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("FERRY_TRUST", trust.join(" ")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect())
    }

    fn device(&self, token: &[u8]) -> Option<EndpointId> {
        let tokens = self.tokens.lock().unwrap();
        tokens
            .iter()
            .find(|(_, t)| t.as_bytes() == token)
            .map(|(id, _)| *id)
    }

    /// Слушать `127.0.0.1` и гнать каждую сессию `ferry ssh` к её заказчику: `client` — клиент
    /// связанного устройства на связи.
    pub async fn serve(
        self: std::sync::Arc<Self>,
        listener: TcpListener,
        client: impl Fn(EndpointId) -> Option<DeviceClient> + Send + Sync + 'static,
    ) -> std::io::Result<()> {
        let client = std::sync::Arc::new(client);
        loop {
            let (stream, _) = listener.accept().await?;
            let (local, client) = (self.clone(), client.clone());
            tokio::spawn(async move {
                let _ = local.session(stream, |id| client(id)).await;
            });
        }
    }

    /// Одна сессия `ferry ssh`: поля «u32 LE длина + байты» — токен, `user@host`, порт, команда,
    /// `GIT_PROTOCOL`; ответ кадрами «байт вида + поле»: 0 — принято, 1 — stdout, 2 — stderr,
    /// 3 — код выхода. После «принято» клиент шлёт stdin полями, пустое поле — конец.
    async fn session(
        &self,
        stream: tokio::net::TcpStream,
        client: impl Fn(EndpointId) -> Option<DeviceClient>,
    ) -> std::io::Result<()> {
        let (mut read, mut write) = stream.into_split();
        let token = read_field(&mut read).await?;
        let mut text = async || {
            read_field(&mut read)
                .await
                .map(|f| String::from_utf8_lossy(&f).into_owned())
        };
        let (target, port, command, protocol) =
            (text().await?, text().await?, text().await?, text().await?);
        let Some(client) = self.device(&token).and_then(client) else {
            send(
                &mut write,
                2,
                "ferry relay: устройство, запустившее команду, не на связи\n".as_bytes(),
            )
            .await?;
            return send(&mut write, 3, &1i32.to_le_bytes()).await;
        };
        let request = GitSsh {
            target,
            port,
            command,
            protocol,
        };
        let Ok((stdin, mut frames)) = client.0.bidi_streaming(request, 16, 16).await else {
            send(
                &mut write,
                2,
                "ferry relay: связь с устройством оборвалась\n".as_bytes(),
            )
            .await?;
            return send(&mut write, 3, &1i32.to_le_bytes()).await;
        };
        // stdin — отдельно и без ожидания: клиент может держать его открытым до кода выхода.
        let pump = tokio::spawn(async move {
            while let Ok(chunk) = read_field(&mut read).await {
                let end = chunk.is_empty();
                if stdin.send(chunk).await.is_err() || end {
                    break;
                }
            }
        });
        loop {
            match frames.recv().await {
                Ok(Some(Relay::Accepted)) => send(&mut write, 0, &[]).await?,
                Ok(Some(Relay::Out(data))) => send(&mut write, 1, &data).await?,
                Ok(Some(Relay::Err(data))) => send(&mut write, 2, &data).await?,
                Ok(Some(Relay::Exit(code))) => {
                    send(&mut write, 3, &code.to_le_bytes()).await?;
                    break;
                }
                Ok(None) | Err(_) => {
                    send(
                        &mut write,
                        2,
                        "ferry relay: связь с устройством оборвалась\n".as_bytes(),
                    )
                    .await?;
                    send(&mut write, 3, &1i32.to_le_bytes()).await?;
                    break;
                }
            }
        }
        pump.abort();
        Ok(())
    }
}

/// Сторона заказчика: выполнить ssh-команду устройства своим ssh, если она только читает и хост
/// доверен (`<dir>/trust`).
pub(crate) async fn serve_git_ssh(
    dir: &Path,
    me: &Status,
    request: GitSsh,
    tx: mpsc::Sender<Relay>,
    mut stdin_rx: mpsc::Receiver<Vec<u8>>,
) {
    let refuse = async |why: String| {
        tx.send(Relay::Err(format!("ferry relay: {why}\n").into_bytes()))
            .await
            .ok();
        tx.send(Relay::Exit(1)).await.ok();
    };
    if me.mode == Mode::Paused {
        return refuse(format!("{} отключён от других устройств", me.name)).await;
    }
    let GitSsh {
        target,
        port,
        command,
        protocol,
    } = request;
    if let Err(why) = relay::allowed(dir, &target, &port, &command, &protocol) {
        return refuse(why).await;
    }
    let mut ssh =
        tokio::process::Command::from(relay::ssh_command(&target, &port, &command, &protocol));
    ssh.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match ssh.spawn() {
        Ok(child) => child,
        Err(e) => {
            tx.send(Relay::Err(
                format!("ferry relay: ssh у {}: {e}\n", me.name).into_bytes(),
            ))
            .await
            .ok();
            tx.send(Relay::Exit(127)).await.ok();
            return;
        }
    };
    tx.send(Relay::Accepted).await.ok();
    let mut stdin = child.stdin.take().unwrap();
    tokio::spawn(async move {
        while let Ok(Some(chunk)) = stdin_rx.recv().await {
            if chunk.is_empty() || stdin.write_all(&chunk).await.is_err() {
                break;
            }
        }
    });
    let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    tokio::join!(forward(out, &tx, Relay::Out), forward(err, &tx, Relay::Err));
    let code = child.wait().await.map(exit_code).unwrap_or(255);
    tx.send(Relay::Exit(code)).await.ok();
}

async fn forward(
    mut from: impl AsyncRead + Unpin,
    tx: &mpsc::Sender<Relay>,
    wrap: fn(Vec<u8>) -> Relay,
) {
    let mut buf = vec![0u8; 16 * 1024];
    while let Ok(n) = from.read(&mut buf).await {
        if n == 0 || tx.send(wrap(buf[..n].to_vec())).await.is_err() {
            break;
        }
    }
}

async fn read_field(r: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
    let len = r.read_u32_le().await?;
    if len > MAX_FIELD {
        return Err(std::io::Error::other("слишком длинное поле"));
    }
    let mut data = vec![0u8; len as usize];
    r.read_exact(&mut data).await?;
    Ok(data)
}

async fn send(w: &mut (impl AsyncWrite + Unpin), kind: u8, data: &[u8]) -> std::io::Result<()> {
    w.write_all(&[kind]).await?;
    w.write_all(&(data.len() as u32).to_le_bytes()).await?;
    w.write_all(data).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restart_rotates_credentials_and_does_not_restore_removed_peers() {
        let peer = iroh::SecretKey::generate().public();
        let removed = iroh::SecretKey::generate().public();
        let dir = std::env::temp_dir().join(format!("ferry-relay-{peer}"));
        let (first, _listener) = Local::bind(&dir, &[peer, removed]).await.unwrap();
        let old = first.token(peer).unwrap();
        let removed_token = first.token(removed).unwrap();
        let (second, _listener) = Local::bind(&dir, &[peer]).await.unwrap();
        assert_eq!(second.device(old.as_bytes()), None);
        assert_eq!(second.device(removed_token.as_bytes()), None);
        let descriptor =
            std::fs::read_to_string(dir.join("relays").join(peer.to_string())).unwrap();
        let (port, token) = descriptor.trim().split_once(' ').unwrap();
        assert_eq!(port.parse::<u16>().unwrap(), second.port);
        assert_eq!(second.device(token.as_bytes()), Some(peer));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
