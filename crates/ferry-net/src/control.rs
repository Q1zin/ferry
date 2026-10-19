//! Сервер локального управления ([`ferry_core::control`]): CLI на этой машине → агент.

use std::path::PathBuf;

use anyhow::Result;
use ferry_core::control::{Envelope, Request, Response};
use irpc::channel::mpsc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::net::tcp::OwnedWriteHalf;

use crate::Agent;
use crate::proto::{Fail, Output};

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
            let (read, write) = stream.into_split();
            let mut read = BufReader::new(read);
            let mut out = Lines(write);
            let mut line = String::new();
            if read.read_line(&mut line).await.is_err() {
                return;
            }
            let request = match serde_json::from_str::<Envelope>(&line) {
                Ok(envelope) if envelope.token == token => envelope.request,
                Ok(_) => return out.error(Fail::new("неверный токен")).await,
                // Чаще всего CLI новее приложения и просит то, чего оно ещё не знает.
                Err(e) => {
                    let text = format!(
                        "приложение ferry {} не понимает запрос ({e}) — CLI новее? Обнови приложение: ferry update",
                        ferry_core::VERSION
                    );
                    return out.error(Fail::new(text)).await;
                }
            };
            // CLI больше ничего не пишет: конец входа — его закрыли (Ctrl-C, таймаут вызвавшего).
            let gone = async move {
                let mut byte = [0u8; 1];
                let _ = read.read(&mut byte).await;
            };
            handle(&agent, request, &mut out, gone).await;
        });
    }
}

/// Ответ построчно.
struct Lines(OwnedWriteHalf);

impl Lines {
    /// Записать строку; `false` — CLI ушёл.
    async fn send(&mut self, response: &Response) -> bool {
        let mut line = serde_json::to_string(response).unwrap_or_default();
        line.push('\n');
        self.0.write_all(line.as_bytes()).await.is_ok()
    }

    async fn error(&mut self, fail: Fail) {
        self.send(&Response::Error(fail)).await;
    }
}

/// Ответ на запрос: разовый или поток вывода (`Run`, `Log`).
enum Answer {
    Once(Box<Response>),
    Stream(mpsc::Receiver<Output>),
}

async fn handle(agent: &Agent, request: Request, out: &mut Lines, gone: impl Future<Output = ()>) {
    match answer(agent, request).await {
        Ok(Answer::Once(response)) => {
            out.send(&response).await;
        }
        Ok(Answer::Stream(rx)) => stream(rx, out, gone).await,
        Err(fail) => out.error(fail).await,
    }
}

async fn answer(agent: &Agent, request: Request) -> Result<Answer, Fail> {
    let response = match request {
        Request::State => Response::State(agent.snapshot()),
        Request::OpenPairing => Response::Pairing(agent.open_pairing()),
        Request::Pair { code } => {
            let peer = agent
                .pair(&code)
                .await
                .map_err(|e| Fail::new(format!("{e:#}")))?;
            Response::Peer(peer)
        }
        Request::SetRole { device, role } => {
            let target = device
                .map(|who| agent.device(&who).map(|d| d.id))
                .transpose()?;
            agent
                .set_role(target, role)
                .await
                .map_err(|e| Fail::new(format!("{e:#}")))?;
            Response::Done
        }
        Request::Sync {
            device,
            root,
            project,
            commit,
            tree,
        } => {
            let extra: Vec<PathBuf> = ferry_core::project::load(&project)?
                .files
                .into_iter()
                .map(PathBuf::from)
                .collect();
            let device = agent.device(&device)?;
            Response::Message(device.sync(&root, &project, &commit, &tree, &extra).await?)
        }
        Request::Run {
            device,
            cmd,
            dir,
            desk,
        } => {
            return Ok(Answer::Stream(
                agent.device(&device)?.run(&cmd, dir, desk).await?,
            ));
        }
        Request::Log {
            device,
            name,
            follow,
        } => {
            return Ok(Answer::Stream(
                agent.device(&device)?.log(&name, follow).await?,
            ));
        }
        Request::Start {
            device,
            name,
            cmd,
            dir,
        } => Response::Message(agent.device(&device)?.start(&name, &cmd, dir).await?),
        Request::Jobs { device } => Response::Jobs(agent.device(&device)?.jobs().await?),
        Request::Kill { device, name } => {
            Response::Message(agent.device(&device)?.kill(&name).await?)
        }
        Request::Shot { device, file } => {
            agent.device(&device)?.shot(&file).await?;
            Response::Done
        }
        Request::ShotWindow { device, file, name } => {
            agent.device(&device)?.shot_window(&file, &name).await?;
            Response::Done
        }
        Request::ShotArea { device, file, area } => {
            agent.device(&device)?.shot_area(&file, area).await?;
            Response::Done
        }
        Request::Input { device, args } => {
            Response::Message(agent.device(&device)?.input(args).await?)
        }
        Request::Rec {
            device,
            seconds,
            fps,
            dest,
        } => Response::Files(agent.device(&device)?.rec(&seconds, &fps, &dest).await?),
        Request::Info { device } => Response::Info(agent.device(&device)?.info().await?),
        Request::Power { device, action } => {
            Response::Message(agent.device(&device)?.power(&action).await?)
        }
        Request::Stop { device } => Response::Message(agent.device(&device)?.stop().await?),
        Request::Put {
            device,
            base,
            rels,
            dir,
        } => {
            agent.device(&device)?.put(&base, &rels, dir).await?;
            Response::Done
        }
        Request::Get {
            device,
            path,
            dir,
            dest,
        } => Response::Files(agent.device(&device)?.get(&path, dir, &dest).await?),
        Request::Pull {
            device,
            root,
            project,
        } => Response::Message(agent.device(&device)?.pull(&root, &project).await?),
        Request::Tunnel {
            device,
            port,
            close: false,
        } => Response::Message(agent.open_tunnel(&device, port).await?),
        Request::Tunnel {
            device,
            port,
            close: true,
        } => Response::Message(agent.close_tunnel(&device, port)?),
        Request::Tunnels => Response::Tunnels(agent.tunnels()),
        Request::Update { device: None } => Response::Message(agent.update_self().await?),
        Request::Update {
            device: Some(device),
        } => Response::Message(agent.device(&device)?.update().await?),
        Request::Logs {
            device: None,
            lines,
        } => Response::Message(agent.logs_self(lines)?),
        Request::Logs {
            device: Some(device),
            lines,
        } => Response::Message(agent.device(&device)?.logs(lines).await?),
        Request::PortOpen { device, port } => {
            Response::Flag(agent.device(&device)?.port_open(port).await?)
        }
        Request::Forget { device } => {
            let snapshot = agent.snapshot();
            let id = snapshot.find(&device).map_err(Fail::new)?.id.parse();
            agent
                .forget(id.map_err(Fail::new)?)
                .map_err(|e| Fail::new(format!("{e:#}")))?;
            Response::Done
        }
    };
    Ok(Answer::Once(Box::new(response)))
}

/// Вывод команды — строками, пока не придёт код выхода или CLI не уйдёт. Ушёл — приёмник
/// бросается, и устройство останавливает команду.
async fn stream(mut rx: mpsc::Receiver<Output>, out: &mut Lines, gone: impl Future<Output = ()>) {
    tokio::pin!(gone);
    loop {
        let next = tokio::select! {
            next = rx.recv() => next,
            _ = &mut gone => return,
        };
        let (response, last) = match next {
            Ok(Some(Output::Out(data))) => (Response::Out(data), false),
            Ok(Some(Output::Err(data))) => (Response::Err(data), false),
            Ok(Some(Output::Exit(code))) => (Response::Exit(code), true),
            Ok(Some(Output::Fail(fail))) => (Response::Error(fail), true),
            Ok(None) | Err(_) => (
                Response::Error(Fail::new("связь с устройством оборвалась посреди команды")),
                true,
            ),
        };
        if !out.send(&response).await || last {
            return;
        }
    }
}
