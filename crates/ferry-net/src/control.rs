//! Сервер локального управления ([`ferry_core::control`]): CLI на этой машине → агент.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use ferry_core::control::{Envelope, Request, Response};
use irpc::channel::mpsc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::net::tcp::OwnedWriteHalf;

use crate::Agent;
use crate::proto::{DeskAction, Fail, Output};

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
            let (request, session) = match serde_json::from_str::<Envelope>(&line) {
                Ok(envelope) if envelope.token == token => (envelope.request, envelope.session),
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
            handle(&agent, request, &session, &mut out, gone).await;
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

async fn handle(
    agent: &Agent,
    request: Request,
    session: &str,
    out: &mut Lines,
    gone: impl Future<Output = ()>,
) {
    match answer(agent, request, session).await {
        Ok(Answer::Once(response)) => {
            out.send(&response).await;
        }
        Ok(Answer::Stream(rx)) => stream(rx, out, gone).await,
        Err(fail) => out.error(fail).await,
    }
}

/// Ввод, который меняет что-то на рабочем столе устройства: под ним — бронь. Окна, элементы,
/// текст на экране и чтение буфера только смотрят.
fn acts(args: &[String]) -> bool {
    let first = args.first().map_or("", String::as_str);
    let reads = [
        "windows",
        "window",
        "ui",
        "text",
        "probe",
        "probe-window",
        "probe-text",
    ];
    !(reads.contains(&first) || args == ["clip"])
}

async fn answer(agent: &Agent, request: Request, session: &str) -> Result<Answer, Fail> {
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
            let device = agent.device(&device)?;
            if !desk {
                return Ok(Answer::Stream(device.run(&cmd, dir, false).await?));
            }
            device.hold(session).await?;
            let rx = device.run(&cmd, dir, true).await?;
            return Ok(Answer::Stream(holding(device, session.into(), rx)));
        }
        Request::Log {
            device,
            name,
            follow,
        } => {
            let rx = agent.device(&device)?.log(&name, follow).await?;
            if !follow {
                return Ok(Answer::Stream(rx));
            }
            let (agent, who) = (agent.clone(), device.clone());
            let again = move || {
                let (agent, device, name) = (agent.clone(), device.clone(), name.clone());
                async move { agent.device(&device)?.log(&name, true).await }
            };
            return Ok(Answer::Stream(resume(who, rx, again)));
        }
        Request::Start {
            device,
            name,
            cmd,
            dir,
        } => {
            // Задача может открыть окна — она тоже под бронью.
            let device = agent.device(&device)?;
            device.hold(session).await?;
            Response::Message(device.start(&name, &cmd, dir).await?)
        }
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
            let device = agent.device(&device)?;
            if acts(&args) {
                device.hold(session).await?;
            }
            Response::Message(device.input(args).await?)
        }
        Request::Desk {
            device,
            action,
            minutes,
        } => {
            let action = match action.as_str() {
                "take" => DeskAction::Hold(minutes.saturating_mul(60)),
                "free" => DeskAction::Free,
                "force" => DeskAction::ForceFree,
                _ => DeskAction::Show,
            };
            Response::Message(agent.device(&device)?.desk(session, action).await?)
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
        Request::Crashes {
            device,
            process,
            last,
        } => Response::Message(agent.device(&device)?.crashes(&process, last).await?),
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
/// Пока идёт `run --desk`, бронь продлевается: окна команды (GUI-тесты) — тоже стол, хоть ввода
/// отсюда и нет.
fn holding(
    device: crate::Device,
    session: String,
    mut rx: mpsc::Receiver<Output>,
) -> mpsc::Receiver<Output> {
    let (tx, out) = mpsc::channel(64);
    // Отдельной задачей: `recv` удалённого потока нельзя прерывать посреди сообщения.
    let renew = tokio::spawn(async move {
        let mut every = tokio::time::interval(crate::desk::HOLD / 3);
        every.tick().await;
        loop {
            every.tick().await;
            let _ = device.hold(&session).await;
        }
    });
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                next = rx.recv() => next,
                _ = tx.closed() => break,
            };
            // Обрыв — дальше как без продления: `stream` скажет, что связь оборвалась.
            let Ok(Some(output)) = next else { break };
            if tx.send(output).await.is_err() {
                break;
            }
        }
        renew.abort();
    });
    out
}

/// Сколько ждать, пока устройство вернётся, прежде чем бросить `log -f`.
const COMEBACK: Duration = Duration::from_secs(15 * 60);
/// Как часто проверять, вернулось ли.
const RETRY: Duration = Duration::from_secs(3);

/// `log -f`, который переживает обрыв связи (перезагрузка, сон, смена сети): ждёт, пока
/// устройство вернётся, и продолжает с того места, где оборвался. `again` открывает лог заново —
/// с начала, уже отданное пропускается.
fn resume<F, Fut>(
    device: String,
    mut rx: mpsc::Receiver<Output>,
    mut again: F,
) -> mpsc::Receiver<Output>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<mpsc::Receiver<Output>, Fail>> + Send,
{
    let (tx, out) = mpsc::channel(64);
    tokio::spawn(async move {
        let note = |text: String| Output::Err(format!("ferry: {text}\n").into_bytes());
        let mut sent = 0;
        loop {
            let mut skip = sent;
            loop {
                let next = tokio::select! {
                    next = rx.recv() => next,
                    _ = tx.closed() => return,
                };
                let output = match next {
                    Ok(Some(Output::Out(mut data))) => {
                        let old = skip.min(data.len());
                        skip -= old;
                        data.drain(..old);
                        if data.is_empty() {
                            continue;
                        }
                        sent += data.len();
                        Output::Out(data)
                    }
                    Ok(Some(output)) => output,
                    Ok(None) | Err(_) => break,
                };
                let last = matches!(output, Output::Exit(_) | Output::Fail(_));
                if tx.send(output).await.is_err() || last {
                    return;
                }
            }
            let lost = note(format!("связь с {device} оборвалась — жду, пока вернётся"));
            if tx.send(lost).await.is_err() {
                return;
            }
            let since = Instant::now();
            rx = loop {
                let attempt = tokio::select! {
                    _ = tx.closed() => return,
                    attempt = async {
                        tokio::time::sleep(RETRY).await;
                        tokio::time::timeout(RETRY * 10, again()).await
                    } => attempt,
                };
                if let Ok(Ok(rx)) = attempt {
                    break rx;
                }
                if since.elapsed() > COMEBACK {
                    let fail = format!("{device} не вернулось за {} мин", COMEBACK.as_secs() / 60);
                    tx.send(Output::Fail(Fail::new(fail))).await.ok();
                    return;
                }
            };
            if tx
                .send(note(format!("{device} снова на связи — продолжаю")))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    out
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn log_follow_resumes_after_the_device_comes_back() {
        let (tx, first) = mpsc::channel(4);
        tx.send(Output::Out(b"step 1\nst".to_vec())).await.unwrap();
        drop(tx);
        let mut tries = 0;
        let again = move || {
            tries += 1;
            let tries = tries;
            async move {
                if tries == 1 {
                    return Err(Fail::new("ещё не на связи"));
                }
                // Лог читается заново с начала, а задача успела дописать.
                let (tx, rx) = mpsc::channel(4);
                tx.send(Output::Out(b"step".to_vec())).await.unwrap();
                tx.send(Output::Out(b" 1\nstep 2\n".to_vec()))
                    .await
                    .unwrap();
                tx.send(Output::Exit(0)).await.unwrap();
                Ok(rx)
            }
        };
        let mut rx = resume("win".into(), first, again);
        let mut out = Vec::new();
        let mut notes = String::new();
        while let Ok(Some(output)) = rx.recv().await {
            match output {
                Output::Out(data) => out.extend(data),
                Output::Err(data) => notes += &String::from_utf8_lossy(&data),
                Output::Exit(code) => {
                    assert_eq!(code, 0);
                    break;
                }
                Output::Fail(fail) => panic!("{fail:?}"),
            }
        }
        assert_eq!(String::from_utf8(out).unwrap(), "step 1\nstep 2\n");
        assert_eq!(
            notes,
            "ferry: связь с win оборвалась — жду, пока вернётся\nferry: win снова на связи — продолжаю\n"
        );
    }
}
