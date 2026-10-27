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
    let sessions = std::sync::Arc::new(Sessions::default());
    tokio::spawn(sessions.clone().watch(agent.clone()));
    tokio::spawn(crate::ownership::watch(agent.clone()));
    loop {
        let (stream, _) = listener.accept().await?;
        let sessions = sessions.clone();
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
            handle(&agent, request, &session, &mut out, gone, &sessions).await;
        });
    }
}

#[derive(Default)]
struct Sessions(std::sync::Mutex<std::collections::BTreeMap<(iroh::EndpointId, String), Instant>>);

impl Sessions {
    async fn hold(&self, device: &crate::Device, session: &str) -> Result<(), Fail> {
        device.hold(session).await?;
        self.track(device.id, session, crate::desk::HOLD.as_secs() as u32);
        Ok(())
    }

    fn track(&self, device: iroh::EndpointId, session: &str, secs: u32) {
        if !ferry_core::session::managed(session) {
            return;
        }
        let until = Instant::now() + Duration::from_secs(secs.into());
        self.0
            .lock()
            .unwrap()
            .entry((device, session.into()))
            .and_modify(|previous| *previous = (*previous).max(until))
            .or_insert(until);
    }

    async fn watch(self: std::sync::Arc<Self>, agent: Agent) {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let tracked = {
                let mut entries = self.0.lock().unwrap();
                entries.retain(|_, until| *until > Instant::now());
                entries.clone()
            };
            if tracked.is_empty() {
                continue;
            }
            let names = tracked
                .keys()
                .map(|(_, name)| name.clone())
                .collect::<Vec<_>>();
            let Ok(Ok(ended)) =
                tokio::task::spawn_blocking(move || ferry_core::control::ended_sessions(&names))
                    .await
            else {
                continue;
            };
            for ((id, session), until) in tracked {
                if !ended.contains(&session) {
                    continue;
                }
                if id == agent.id() {
                    let _ = agent.desk_self(&session, DeskAction::Free);
                    self.0.lock().unwrap().remove(&(id, session));
                    continue;
                }
                let Ok(device) = agent.device(&id.to_string()) else {
                    continue;
                };
                // Free проверяет владельца: новую бронь другой сессии не снимаем.
                if let Ok(Ok(_)) = tokio::time::timeout(
                    Duration::from_secs(3),
                    device.desk(&session, DeskAction::Free),
                )
                .await
                {
                    let mut entries = self.0.lock().unwrap();
                    if entries.get(&(id, session.clone())) == Some(&until) {
                        entries.remove(&(id, session));
                    }
                }
            }
        }
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
    Frames(mpsc::Receiver<crate::proto::Reply<ferry_core::screen::live::Frame>>),
}

async fn handle(
    agent: &Agent,
    request: Request,
    session: &str,
    out: &mut Lines,
    gone: impl Future<Output = ()>,
    sessions: &std::sync::Arc<Sessions>,
) {
    match answer(agent, request, session, sessions).await {
        Ok(Answer::Once(response)) => {
            out.send(&response).await;
        }
        Ok(Answer::Stream(rx)) => stream(rx, out, gone).await,
        Ok(Answer::Frames(mut rx)) => {
            let relay = async {
                loop {
                    match rx.recv().await {
                        Ok(Some(Ok(frame))) => {
                            if !out.send(&Response::Frame(frame)).await {
                                break;
                            }
                        }
                        Ok(Some(Err(e))) => {
                            out.error(e).await;
                            break;
                        }
                        _ => {
                            out.error(Fail::new("трансляция прервана")).await;
                            break;
                        }
                    }
                }
            };
            // On disconnect drop the whole channel; never resume a cancelled irpc recv.
            tokio::select! { _ = relay => {}, _ = gone => {} }
        }
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

async fn answer(
    agent: &Agent,
    request: Request,
    session: &str,
    sessions: &std::sync::Arc<Sessions>,
) -> Result<Answer, Fail> {
    let (request, shell) = match request {
        Request::RunShell {
            device,
            cmd,
            dir,
            desk,
            shell,
        } => (
            Request::Run {
                device,
                cmd,
                dir,
                desk,
            },
            shell,
        ),
        Request::StartShell {
            device,
            name,
            cmd,
            dir,
            shell,
        } => (
            Request::Start {
                device,
                name,
                cmd,
                dir,
            },
            shell,
        ),
        request => (request, ferry_core::desk::Shell::Default),
    };
    let response = match request {
        Request::RunShell { .. } | Request::StartShell { .. } => unreachable!(),
        Request::ScreenStream { device, request } => {
            let rx = match device {
                Some(who) => agent.device(&who)?.screen_stream(request).await?,
                None => agent.screen_stream_self(request).await,
            };
            return Ok(Answer::Frames(rx));
        }
        Request::Storage { device, request } => Response::Report(match device {
            Some(who) => agent.device(&who)?.storage(request).await?,
            None => agent.storage_self(request).await?,
        }),
        Request::Human { device, request } => Response::Report(match device {
            Some(who) => agent.device(&who)?.human(request, session).await?,
            None => agent.human_self(request, session).await?,
        }),
        Request::DeskNote {
            device,
            minutes,
            note,
        } => {
            let action = DeskAction::HoldNote {
                seconds: minutes.saturating_mul(60),
                note,
            };
            let message = if device == "local" {
                sessions.track(agent.id(), session, minutes.saturating_mul(60));
                agent.desk_self(session, action)?
            } else {
                let target = agent.device(&device)?;
                let said = target.desk(session, action).await?;
                sessions.track(target.id, session, minutes.saturating_mul(60));
                said
            };
            Response::Message(message)
        }
        Request::Session { request } => Response::Report(
            ferry_core::session::execute(&ferry_core::ferry_home()?, request)?.to_string(),
        ),
        Request::OwnedTunnel {
            device,
            port,
            close,
            keep,
        } => {
            if close {
                if session.is_empty() {
                    Response::Message(agent.close_tunnel(&device, port)?)
                } else {
                    let target = agent.device(&device)?;
                    if !agent.close_tunnel_owned(&target.name, port, session)? {
                        return Err(Fail::refused(
                            "туннель не принадлежит этой сессии или уже закрыт",
                        ));
                    }
                    Response::Message(format!("ferry: туннель {}:{port} закрыт", target.name))
                }
            } else {
                let owner = if keep || session.is_empty() {
                    None
                } else {
                    crate::ownership::validate(session)?;
                    Some(session.to_string())
                };
                let target = agent.device(&device)?;
                let message = agent
                    .open_tunnel_owned(&target.name, port, owner.clone())
                    .await?;
                if owner.is_some() {
                    crate::ownership::tunnel(target.name, session, port)?;
                }
                Response::Message(message)
            }
        }
        Request::AtomicStart {
            device,
            mut request,
            local_cwd,
        } => {
            if request.session.is_empty() {
                request.session = session.into();
            }
            if !request.keep {
                let owner_device = device
                    .as_ref()
                    .map(|who| agent.device(who).map(|d| d.id.to_string()))
                    .transpose()?;
                crate::ownership::job(owner_device, &request.session, &request.request_id)?;
            }
            let desktop = request.resource == ferry_core::jobs::execution::Resource::Desktop;
            let result = match device {
                Some(who) => {
                    if local_cwd.is_some() {
                        return Err(Fail::new("remote start не принимает local_cwd"));
                    }
                    let target = agent.device(&who)?;
                    if desktop {
                        sessions.hold(&target, &request.session).await?;
                    }
                    target.atomic_start(request).await?
                }
                None => {
                    if desktop {
                        agent.desk_self(
                            &request.session,
                            DeskAction::Hold(crate::desk::HOLD.as_secs() as u32),
                        )?;
                        sessions.track(
                            agent.id(),
                            &request.session,
                            crate::desk::HOLD.as_secs() as u32,
                        );
                    }
                    agent.atomic_start_self(request, local_cwd).await?
                }
            };
            Response::Report(serde_json::to_string(&result).map_err(Fail::new)?)
        }
        Request::ExecutionCancel { device, request_id } => Response::Report(match device {
            Some(who) => {
                agent
                    .device(&who)?
                    .execution_cancel(request_id, session.into())
                    .await?
            }
            None => {
                agent
                    .execution_cancel_self(request_id, session.into())
                    .await?
            }
        }),
        Request::ExecutionLookup { device, request_id } => {
            let result = match device {
                Some(who) => agent.device(&who)?.execution_lookup(request_id).await?,
                None => agent.execution_lookup_self(request_id).await?,
            };
            Response::Report(serde_json::json!({"started":result}).to_string())
        }
        Request::WorkspaceSync {
            device,
            root,
            project,
            id,
            commit,
            tree,
        } => {
            let source = ferry_core::workspace::source(&project, &id, &commit, &tree)?;
            let state = ferry_core::ferry_home()?;
            let workspace = source.workspace.clone();
            let _guard = tokio::task::spawn_blocking(move || {
                ferry_core::workspace::lock(&state, &workspace)
            })
            .await
            .map_err(Fail::new)??;
            let extra: Vec<PathBuf> = ferry_core::project::load(&project)?
                .files
                .into_iter()
                .map(PathBuf::from)
                .collect();
            let result = match device {
                Some(who) => {
                    let target = agent.device(&who)?;
                    let status = target
                        .workspace(ferry_core::workspace::Request {
                            source: source.clone(),
                            action: ferry_core::workspace::Action::Prepare,
                        })
                        .await?;
                    let status: serde_json::Value =
                        serde_json::from_str(&status).map_err(Fail::new)?;
                    if status["ready"] == true {
                        status.to_string()
                    } else {
                        target
                            .sync(&root, &source.workspace, &commit, &tree, &extra)
                            .await?;
                        target
                            .workspace(ferry_core::workspace::Request {
                                source: source.clone(),
                                action: ferry_core::workspace::Action::Seal,
                            })
                            .await?
                    }
                }
                None => {
                    let home = ferry_core::home()?;
                    let value =
                        ferry_core::workspace::prepare_local(&home, &root, &source, &extra)?;
                    value.to_string()
                }
            };
            Response::Report(result)
        }
        Request::WorkspacePatch {
            device,
            root,
            source,
            apply,
        } => {
            ferry_core::workspace::validate(&source)?;
            let saved = match device {
                Some(who) => agent.device(&who)?.workspace_patch(&source).await?,
                None => {
                    let home = ferry_core::home()?;
                    let cwd = home.join("ferry").join(&source.workspace);
                    ferry_core::workspace::verify(&cwd, &source)?;
                    let script = ferry_core::sync::changes_script(&source.workspace, "");
                    let output = ferry_core::desk::shell(&script)
                        .current_dir(&home)
                        .output()?;
                    if !output.status.success() {
                        return Err(Fail::new(String::from_utf8_lossy(&output.stderr)));
                    }
                    let path = ferry_core::ferry_home()?.join("patches").join(format!(
                        "{}-{}.patch",
                        source.workspace,
                        ferry_core::jobs::unique_id()
                    ));
                    std::fs::create_dir_all(path.parent().unwrap())?;
                    std::fs::write(&path, output.stdout)?;
                    path
                }
            };
            Response::Report(
                ferry_core::workspace::patch(&root, &saved, apply.as_deref())?.to_string(),
            )
        }
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
                return Ok(Answer::Stream(
                    device.run_shell(&cmd, dir, false, shell).await?,
                ));
            }
            sessions.hold(&device, session).await?;
            let rx = device.run_shell(&cmd, dir, true, shell).await?;
            return Ok(Answer::Stream(holding(
                device,
                session.into(),
                rx,
                sessions.clone(),
            )));
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
            sessions.hold(&device, session).await?;
            Response::Message(device.start_shell(&name, &cmd, dir, shell).await?)
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
                sessions.hold(&device, session).await?;
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
                "release-owned" => DeskAction::ReleaseOwned,
                "force" => DeskAction::ForceFree,
                _ => DeskAction::Show,
            };
            if device == "local" {
                if let DeskAction::Hold(secs) = &action {
                    sessions.track(agent.id(), session, *secs);
                }
                return Ok(Answer::Once(Box::new(Response::Message(
                    agent.desk_self(session, action)?,
                ))));
            }
            let device = agent.device(&device)?;
            let message = device.desk(session, action.clone()).await?;
            if let DeskAction::Hold(secs) = action {
                sessions.track(device.id, session, secs);
            }
            Response::Message(message)
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
        Request::ActV2 {
            device,
            request,
            file,
        } => {
            let (mut result, lease) = match device {
                Some(who) => {
                    let device = agent.device(&who)?;
                    sessions.hold(&device, session).await?;
                    let lease = device.desk(session, DeskAction::Show).await?;
                    (device.act_v2(*request).await?, lease)
                }
                None => {
                    agent.desk_self(
                        session,
                        DeskAction::Hold(crate::desk::HOLD.as_secs() as u32),
                    )?;
                    sessions.track(agent.id(), session, crate::desk::HOLD.as_secs() as u32);
                    (
                        agent.act_v2_self(*request).await?,
                        agent.desk_self(session, DeskAction::Show)?,
                    )
                }
            };
            let observation = result.observation.take();
            let mut value = serde_json::to_value(result).map_err(|e| Fail::new(e.to_string()))?;
            if let Some(o) = observation {
                value["observation"] = observation_v2_value(o, &file, lease)?;
            }
            Response::Report(value.to_string())
        }
        Request::ObserveV2 {
            device,
            request,
            file,
        } => {
            let (observation, lease) = match device {
                Some(who) => {
                    let device = agent.device(&who)?;
                    sessions.hold(&device, session).await?;
                    let lease = device.desk(session, DeskAction::Show).await?;
                    (device.observe_v2(request).await?, lease)
                }
                None => {
                    agent.desk_self(
                        session,
                        DeskAction::Hold(crate::desk::HOLD.as_secs() as u32),
                    )?;
                    sessions.track(agent.id(), session, crate::desk::HOLD.as_secs() as u32);
                    (
                        agent.observe_v2_self(request).await?,
                        agent.desk_self(session, DeskAction::Show)?,
                    )
                }
            };
            Response::Report(observation_v2_value(observation, &file, lease)?.to_string())
        }
        Request::Act {
            device,
            request,
            file,
        } => {
            let (mut result, lease) = match device {
                Some(who) => {
                    let device = agent.device(&who)?;
                    sessions.hold(&device, session).await?;
                    let lease = device.desk(session, DeskAction::Show).await?;
                    (device.act(request).await?, lease)
                }
                None => {
                    agent.desk_self(
                        session,
                        DeskAction::Hold(crate::desk::HOLD.as_secs() as u32),
                    )?;
                    sessions.track(agent.id(), session, crate::desk::HOLD.as_secs() as u32);
                    (
                        agent.act_self(request).await?,
                        agent.desk_self(session, DeskAction::Show)?,
                    )
                }
            };
            let observation = result.observation.take();
            let mut value = serde_json::to_value(result).map_err(|e| Fail::new(e.to_string()))?;
            if let Some(o) = observation {
                value["observation"] = observation_value(o, &file, lease)?;
            }
            Response::Report(value.to_string())
        }
        Request::Observe {
            device,
            request,
            file,
        } => {
            let (observation, lease) = match device {
                Some(who) => {
                    let device = agent.device(&who)?;
                    sessions.hold(&device, session).await?;
                    let lease = device.desk(session, DeskAction::Show).await?;
                    (device.observe(request).await?, lease)
                }
                None => {
                    agent.desk_self(
                        session,
                        DeskAction::Hold(crate::desk::HOLD.as_secs() as u32),
                    )?;
                    sessions.track(agent.id(), session, crate::desk::HOLD.as_secs() as u32);
                    (
                        agent.observe_self(request).await?,
                        agent.desk_self(session, DeskAction::Show)?,
                    )
                }
            };
            Response::Report(observation_value(observation, &file, lease)?.to_string())
        }
        Request::Evidence { device, query } => Response::Report(match device {
            Some(who) => agent.device(&who)?.evidence(query).await?,
            None => agent.evidence_self(query).await?,
        }),
        Request::Capabilities { device, request } => {
            Response::Capabilities(agent.device(&device)?.capabilities(request).await?)
        }
        Request::JobResult {
            device,
            name,
            cursor,
            max_bytes,
            wait,
        } => {
            let request = crate::proto::JobResult {
                name,
                cursor,
                max_bytes,
                wait,
            };
            Response::JobResult(match device {
                Some(device) => agent.device(&device)?.job_result(request).await?,
                None => agent.job_result_self(request).await?,
            })
        }
        Request::UpdateStatus { device } => Response::UpdateStatus(match device {
            None => agent.update_status(),
            Some(device) => agent.device(&device)?.update_status().await?,
        }),
        Request::UpdateFile {
            device,
            file,
            signature,
        } => Response::Message(match device {
            // NSIS не может заменить работающий установленный ferry.exe (сам CLI).
            None if cfg!(windows) => {
                return Err(Fail::new(
                    "обновляй эту Windows с другого устройства: работающий локальный CLI мешает установщику заменить ferry.exe",
                ));
            }
            None => agent.update_file_self(file, signature).await?,
            Some(device) => agent.device(&device)?.update_file(&file, signature).await?,
        }),
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
    sessions: std::sync::Arc<Sessions>,
) -> mpsc::Receiver<Output> {
    let (tx, out) = mpsc::channel(64);
    // Отдельной задачей: `recv` удалённого потока нельзя прерывать посреди сообщения.
    let renew = tokio::spawn(async move {
        let mut every = tokio::time::interval(crate::desk::HOLD / 3);
        every.tick().await;
        loop {
            every.tick().await;
            if ferry_core::control::session_pid(&session).is_some() {
                let names = vec![session.clone()];
                if let Ok(Ok(ended)) =
                    tokio::task::spawn_blocking(move || ferry_core::control::ended_sessions(&names))
                        .await
                    && !ended.is_empty()
                {
                    break;
                }
                sessions.track(device.id, &session, crate::desk::HOLD.as_secs() as u32);
            }
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

fn observation_v2_value(
    mut observation: ferry_core::screen::observe::ObservationV2,
    file: &std::path::Path,
    lease: String,
) -> Result<serde_json::Value, Fail> {
    let png = std::mem::take(&mut observation.png);
    let value = serde_json::to_value(observation).map_err(Fail::new)?;
    save_observation(value, png, file, lease)
}

fn observation_value(
    mut observation: ferry_core::screen::observe::Observation,
    file: &std::path::Path,
    lease: String,
) -> Result<serde_json::Value, Fail> {
    let png = std::mem::take(&mut observation.png);
    let value = serde_json::to_value(observation).map_err(|e| Fail::new(e.to_string()))?;
    save_observation(value, png, file, lease)
}
fn save_observation(
    mut value: serde_json::Value,
    png: Vec<u8>,
    file: &std::path::Path,
    lease: String,
) -> Result<serde_json::Value, Fail> {
    value.as_object_mut().unwrap().remove("png");
    value["coordinates"] = serde_json::json!({"ui":"desktop physical pixels", "ocr":"window screenshot pixels", "window_rect":"x,y,width,height in desktop physical pixels"});
    value["desk"] = lease.into();
    value["screenshot"] = serde_json::Value::Null;
    if !png.is_empty() {
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        options.open(file)?.write_all(&png)?;
        value["screenshot"] = file.to_string_lossy().into_owned().into();
    }
    Ok(value)
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
