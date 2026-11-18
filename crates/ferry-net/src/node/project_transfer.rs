use std::path::PathBuf;

use ferry_core::jobs::execution::{Source, StartRequest, Started};
use ferry_core::project_transfer::{self, Direction, State, Transfer};
use ferry_core::{storage, workspace};
use iroh::EndpointId;
use serde_json::Value;

use super::Node;
use crate::proto::{
    Fail, PROJECT_SESSION, ProjectContents, ProjectTransfer, ProjectTransferAction, Reply,
};

fn transfer_id(source: &Source) -> Reply<&str> {
    workspace::validate(source)?;
    let id = source
        .workspace
        .strip_prefix(&format!("{}--", source.project))
        .ok_or_else(|| Fail::new("неверная копия проекта"))?;
    project_transfer::validate_id(id)?;
    Ok(id)
}

fn verify(transfer: &Transfer, source: &Source, from: EndpointId) -> Reply<()> {
    if transfer.peer_id != from.to_string() || transfer.source.as_ref() != Some(source) {
        return Err(Fail::refused("копия проекта принадлежит другой передаче"));
    }
    Ok(())
}

impl Node {
    pub(super) async fn receive_project(
        &self,
        request: ProjectTransfer,
        from: EndpointId,
    ) -> Reply<String> {
        self.check(false)?;
        let state = self.dir.clone();
        let home = self.home.clone();
        tokio::task::spawn_blocking(move || -> Reply<String> {
            let ProjectTransfer { id, source, action } = request;
            if transfer_id(&source)? != id {
                return Err(Fail::new("ID копии и передачи не совпадают"));
            }
            let _lock = workspace::lock(&state, &format!("transfer-{id}"))?;
            let saved = project_transfer::get(&state, &id, Direction::Incoming)?;
            let mut transfer = match saved {
                Some(saved) => {
                    verify(&saved, &source, from)?;
                    saved
                }
                None if matches!(action, ProjectTransferAction::Prepare { .. }) => {
                    let mut transfer =
                        Transfer::new(&id, Direction::Incoming, from.to_string(), &source.project);
                    transfer.source = Some(source.clone());
                    transfer.state = State::Transferring;
                    project_transfer::save(&state, &transfer)?;
                    transfer
                }
                None => return Err(Fail::new("приём проекта ещё не начинался")),
            };
            let request = |action| workspace::Request {
                source: source.clone(),
                action,
            };
            let state_from_workspace = |data: &Value| {
                if data["ready"] == true {
                    State::Ready
                } else {
                    State::Transferring
                }
            };
            let old_state = transfer.state;
            let perform = || -> Reply<Value> {
                match action {
                    ProjectTransferAction::Prepare { extras, caches } => {
                        let status =
                            workspace::execute(&home, request(workspace::Action::Prepare))?;
                        if status["ready"] != true {
                            storage::save_workspace_caches(
                                &state, &home, &source, &caches, &extras,
                            )?;
                        }
                        transfer.state = state_from_workspace(&status);
                        Ok(status)
                    }
                    ProjectTransferAction::Seal => {
                        let status = workspace::execute(&home, request(workspace::Action::Seal))?;
                        transfer.state = state_from_workspace(&status);
                        Ok(status)
                    }
                    ProjectTransferAction::Failed(message) => {
                        if matches!(
                            old_state,
                            State::Preparing | State::Transferring | State::Interrupted
                        ) {
                            transfer.state = State::Failed;
                            transfer.error = Some(message.chars().take(512).collect());
                        }
                        Ok(Value::Null)
                    }
                }
            };
            let result = perform();
            if matches!(old_state, State::Starting | State::Started) {
                transfer.state = old_state;
            } else if let Ok(value) = &result {
                if !value.is_null() {
                    transfer.error = None;
                    transfer.cwd = value["cwd"].as_str().map(PathBuf::from);
                }
            } else if let Err(error) = &result {
                transfer.state = State::Failed;
                transfer.error = Some(error.to_string());
            }
            transfer.updated = ferry_core::now();
            project_transfer::save(&state, &transfer)?;
            result.map(|data| data.to_string())
        })
        .await
        .map_err(Fail::new)?
    }

    pub(super) async fn project_contents(
        &self,
        request: ProjectContents,
        from: EndpointId,
    ) -> Reply<String> {
        self.check(false)?;
        let state = self.dir.clone();
        let home = self.home.clone();
        tokio::task::spawn_blocking(move || -> Reply<String> {
            let id = transfer_id(&request.source)?;
            let transfer = project_transfer::get(&state, id, Direction::Incoming)?
                .ok_or_else(|| Fail::refused("копия проекта недоступна"))?;
            verify(&transfer, &request.source, from)?;
            Ok(storage::project_contents(&home, &request.source, &request.directory)?.to_string())
        })
        .await
        .map_err(Fail::new)?
    }

    pub(super) fn transfer_for_start(
        &self,
        request: &StartRequest,
        from: EndpointId,
    ) -> Reply<Option<Transfer>> {
        if request.session != PROJECT_SESSION {
            return Ok(None);
        }
        let source = request
            .source
            .as_ref()
            .ok_or_else(|| Fail::new("нужен снимок проекта"))?;
        if transfer_id(source)? != request.request_id {
            return Err(Fail::new("ID запуска должен совпадать с ID передачи"));
        }
        let mut transfer =
            project_transfer::get(&self.dir, &request.request_id, Direction::Incoming)?
                .ok_or_else(|| Fail::refused("копия проекта не готова"))?;
        verify(&transfer, source, from)?;
        if transfer.cwd.is_none() || transfer.state == State::Transferring {
            return Err(Fail::refused("копия проекта ещё не готова"));
        }
        if transfer.state != State::Started {
            transfer.state = State::Starting;
            transfer.error = None;
            transfer.updated = ferry_core::now();
            project_transfer::save(&self.dir, &transfer)?;
        }
        Ok(Some(transfer))
    }

    pub(super) fn record_project_start(&self, mut transfer: Transfer, result: &Reply<Started>) {
        match result {
            Ok(started) => {
                transfer.state = State::Started;
                transfer.run = Some(started.clone());
            }
            Err(error) if transfer.run.is_none() => {
                transfer.state = State::Failed;
                transfer.error = Some(error.to_string());
            }
            Err(_) => return,
        }
        transfer.updated = ferry_core::now();
        if let Err(error) = project_transfer::save(&self.dir, &transfer) {
            tracing::warn!(operation=%transfer.id, %error, "не удалось сохранить квитанцию запуска проекта");
        }
    }
}
