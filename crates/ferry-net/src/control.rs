//! Сервер локального управления ([`ferry_core::control`]): CLI на этой машине → агент.

use std::path::PathBuf;

use anyhow::Result;
use ferry_core::control::{Envelope, Request, Response};
use iroh::EndpointId;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::Agent;

/// Слушать 127.0.0.1 и записать порт и токен в `<dir>/app` (только для владельца).
pub async fn serve(agent: Agent, dir: PathBuf) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("генератор случайных чисел: {e}"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(&dir)?;
    let port = listener.local_addr()?.port();
    ferry_core::write_private(&dir.join("app"), &format!("{port} {token}\n"))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let (agent, token) = (agent.clone(), token.clone());
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            if BufReader::new(read).read_line(&mut line).await.is_err() {
                return;
            }
            let response = match serde_json::from_str::<Envelope>(&line) {
                Ok(envelope) if envelope.token == token => handle(&agent, envelope.request).await,
                Ok(_) => Response::Error("неверный токен".into()),
                Err(e) => Response::Error(format!("запрос: {e}")),
            };
            let mut out = serde_json::to_string(&response).unwrap_or_default();
            out.push('\n');
            let _ = write.write_all(out.as_bytes()).await;
        });
    }
}

async fn handle(agent: &Agent, request: Request) -> Response {
    let result: Result<Response> = async {
        Ok(match request {
            Request::State => Response::State(agent.snapshot()),
            Request::OpenPairing => Response::Pairing(agent.open_pairing()),
            Request::Pair { code } => Response::Peer(agent.pair(&code).await?),
            Request::SetRole { device, role } => {
                let target = device.map(|who| id_of(agent, &who)).transpose()?;
                agent.set_role(target, role).await?;
                Response::Done
            }
        })
    }
    .await;
    result.unwrap_or_else(|e| Response::Error(format!("{e:#}")))
}

/// Связанное устройство по имени или началу ключа.
fn id_of(agent: &Agent, who: &str) -> Result<EndpointId> {
    let snapshot = agent.snapshot();
    let peer = snapshot.find(who).map_err(anyhow::Error::msg)?;
    Ok(peer.id.parse()?)
}
