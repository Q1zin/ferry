use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ferry_core::desk::Shell;
use ferry_core::device::Dir;
use ferry_core::jobs::{self, evidence, execution};
use ferry_core::project_transfer::{self, Direction, State as Phase, Transfer};
use ferry_core::{project, storage, sync, workspace};
use ferry_net::Device;
use ferry_net::proto::{PROJECT_SESSION, ProjectContents, ProjectTransfer, ProjectTransferAction};
use serde_json::{Value, json};
use tauri::State;

use crate::{Agent, Reply, err, on_line};

static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn active() -> &'static Mutex<HashSet<String>> {
    ACTIVE.get_or_init(Mutex::default)
}

struct Worker(String);

impl Drop for Worker {
    fn drop(&mut self) {
        active().lock().unwrap().remove(&self.0);
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Request {
    Inspect {
        path: PathBuf,
    },
    Begin {
        path: PathBuf,
        device: String,
        command: Option<String>,
        #[serde(default)]
        shell: Shell,
    },
    History,
    Result {
        id: String,
        direction: Direction,
    },
}

fn inspect(path: &Path) -> Reply<(sync::Repo, project::Project)> {
    if !path.is_absolute() || !path.is_dir() {
        return Err("Укажи абсолютный путь к папке локального проекта".into());
    }
    let path = path.canonicalize().map_err(err)?;
    let repo = sync::repo_at(&path)
        .map_err(err)?
        .ok_or("В этой папке нет рабочего дерева Git")?;
    let profile = project::for_checkout(&repo).map_err(err)?.1;
    Ok((repo, profile))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Reply<T> + Send + 'static) -> Reply<T> {
    tokio::task::spawn_blocking(f).await.map_err(err)?
}

// Имя каталога на узле должно подходить и для Windows. Уникальность обеспечивает ID передачи.
fn portable_name(name: &str) -> String {
    let name: String = name
        .chars()
        .take(80)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches('-');
    if name.is_empty() {
        "project".into()
    } else {
        name.into()
    }
}

fn save(state: &Path, row: &mut Transfer, phase: Phase) -> Reply<()> {
    row.updated = ferry_core::now();
    row.state = phase;
    project_transfer::save(state, row).map_err(err)
}

async fn materialize(
    state: &Path,
    row: &mut Transfer,
    device: &Device,
    repo: sync::Repo,
    profile: project::Project,
    command: Option<String>,
    shell: Shell,
) -> Reply<()> {
    let (commit, tree, repo) = blocking(move || {
        let (commit, tree) = sync::snapshot(&repo).map_err(err)?;
        Ok((commit, tree, repo))
    })
    .await?;
    let source = workspace::source(&row.project, &row.id, &commit, &tree).map_err(err)?;
    row.source = Some(source.clone());
    save(state, row, Phase::Transferring)?;
    let request = |action| ProjectTransfer {
        id: row.id.clone(),
        source: source.clone(),
        action,
    };
    let reply = device
        .project_transfer(request(ProjectTransferAction::Prepare {
            extras: profile.files.clone(),
            caches: profile.caches,
        }))
        .await
        .map_err(err)?;
    let mut status: Value = serde_json::from_str(&reply).map_err(err)?;
    if status["ready"] != true {
        let extras: Vec<_> = profile.files.into_iter().map(PathBuf::from).collect();
        for extra in &extras {
            if !repo.root.join(extra).exists() {
                return Err(format!(
                    "Не найден дополнительный файл проекта {}",
                    extra.display()
                ));
            }
        }
        device
            .sync(&repo.root, &source.workspace, &commit, &tree, &extras)
            .await
            .map_err(err)?;
        let sealed = device
            .project_transfer(request(ProjectTransferAction::Seal))
            .await
            .map_err(err)?;
        status = serde_json::from_str(&sealed).map_err(err)?;
    }
    if status["ready"] != true {
        return Err("Принимающее устройство не подтвердило снимок".into());
    }
    row.cwd = Some(PathBuf::from(
        status["cwd"]
            .as_str()
            .ok_or("Узел не вернул папку проекта")?,
    ));
    save(state, row, Phase::Ready)?;
    if let Some(command) = command {
        save(state, row, Phase::Starting)?;
        let request = execution::StartRequest {
            name: row.id.clone(),
            request_id: row.id.clone(),
            command,
            dir: Dir::Project {
                name: source.workspace.clone(),
                prefix: repo.prefix,
            },
            shell,
            resource: execution::Resource::Headless,
            source: Some(source),
            session: PROJECT_SESSION.into(),
            keep: true,
            timeout_sec: None,
        };
        let started = match device.atomic_start(request).await {
            Ok(started) => started,
            Err(error) => device
                .execution_lookup(row.id.clone())
                .await
                .ok()
                .flatten()
                .ok_or_else(|| err(error))?,
        };
        row.run = Some(started);
        save(state, row, Phase::Started)?;
    }
    Ok(())
}

#[tauri::command]
pub async fn project_transfer(agent: State<'_, Agent>, request: Request) -> Reply<Value> {
    let state = ferry_core::ferry_home().map_err(err)?;
    match request {
        Request::Inspect { path } => blocking(move || {
            let (repo, profile) = inspect(&path)?;
            Ok(json!({"project":repo.name,"root":repo.root,"prefix":repo.prefix,"extras":profile.files,"caches":profile.caches}))
        }).await,
        Request::Begin { path, device, command, shell } => {
            let device = on_line(&agent, &device)?;
            let (repo, profile) = blocking(move || inspect(&path)).await?;
            let command = command.filter(|command| !command.trim().is_empty());
            if command.as_ref().is_some_and(|command| command.len() > 128 * 1024) {
                return Err("Слишком длинная команда запуска".into());
            }
            let id = format!("gui-{}", jobs::unique_id());
            let mut row = Transfer::new(id, Direction::Outgoing, device.id.to_string(), portable_name(&repo.name));
            let worker = Worker(row.id.clone());
            active().lock().unwrap().insert(row.id.clone());
            save(&state, &mut row, Phase::Preparing)?;
            let initial = serde_json::to_value(&row).map_err(err)?;
            tauri::async_runtime::spawn(async move {
                let _worker = worker;
                if let Err(error) = materialize(&state, &mut row, &device, repo, profile, command, shell).await {
                    if row.state == Phase::Transferring && let Some(source) = &row.source {
                        let outgoing = ProjectTransfer { id: row.id.clone(), source: source.clone(), action: ProjectTransferAction::Failed("Передача не подтверждена отправителем".into()) };
                        let _ = device.project_transfer(outgoing).await;
                    }
                    let phase = if row.state == Phase::Starting { Phase::Interrupted } else { Phase::Failed };
                    row.error = Some(error);
                    if let Err(error) = save(&state, &mut row, phase) {
                        tracing::warn!(id=%row.id, %error, "не удалось обновить историю передачи");
                    }
                }
            });
            Ok(initial)
        }
        Request::History => blocking(move || {
            let mut rows = project_transfer::list(&state).map_err(err)?;
            let active = active().lock().map_err(err)?.clone();
            for row in &mut rows {
                if row.direction == Direction::Outgoing && !active.contains(&row.id) && matches!(row.state, Phase::Preparing | Phase::Transferring | Phase::Starting) {
                    row.state = Phase::Interrupted;
                    row.error = Some("Ferry перезапустился до подтверждения результата".into());
                }
                if row.direction == Direction::Incoming && row.state == Phase::Starting && row.run.is_none()
                    && let Ok(Some(started)) = execution::lookup(&state, &row.id)
                {
                    row.run = Some(started);
                    row.state = Phase::Started;
                }
            }
            Ok(json!({"transfers":rows}))
        }).await,
        Request::Result { id, direction } => {
            let row = project_transfer::get(&state, &id, direction).map_err(err)?.ok_or("Передача не найдена")?;
            let device = if direction == Direction::Outgoing { Some(on_line(&agent, &row.peer_id)?) } else { None };
            let run = match (row.run, &device) {
                (Some(run), _) => run,
                (None, Some(device)) => device.execution_lookup(id).await.map_err(err)?.ok_or("Запуск ещё не подтверждён")?,
                (None, None) => execution::lookup(&state, &id).map_err(err)?.ok_or("Запуск ещё не подтверждён")?,
            };
            let query = evidence::Query { name: run.name, run_id: Some(run.run_id), action: evidence::Action::Result { cursor: None, max_bytes: 64 * 1024 } };
            if let Some(device) = device {
                let result = device.evidence(query).await.map_err(err)?;
                serde_json::from_str(&result).map_err(err)
            } else {
                blocking(move || evidence::execute(&state, query).map_err(err)).await
            }
        }
    }
}

#[tauri::command]
pub async fn project_storage(
    agent: State<'_, Agent>,
    id: Option<String>,
    request: storage::Request,
) -> Reply<Value> {
    let raw = if let Some(id) = id {
        on_line(&agent, &id)?.storage(request).await.map_err(err)?
    } else {
        agent.storage_self(request).await.map_err(err)?
    };
    serde_json::from_str(&raw).map_err(err)
}

#[tauri::command]
pub async fn project_files(
    agent: State<'_, Agent>,
    id: Option<String>,
    source: execution::Source,
    directory: String,
) -> Reply<Value> {
    if let Some(id) = id {
        let raw = on_line(&agent, &id)?
            .project_contents(ProjectContents { source, directory })
            .await
            .map_err(err)?;
        serde_json::from_str(&raw).map_err(err)
    } else {
        blocking(move || {
            storage::project_contents(&ferry_core::home().map_err(err)?, &source, &directory)
                .map_err(err)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_names_are_portable() {
        assert_eq!(portable_name(".my app"), "my-app");
        assert_eq!(portable_name("Проект"), "project");
    }
}
