//! Persisted ownership receipts: ending a session stops only its exact new executions.
use crate::{Agent, Fail};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};
#[derive(Serialize, Deserialize)]
enum Kind {
    Job { request_id: String },
    Tunnel { port: u16 },
}
#[derive(Serialize, Deserialize)]
struct Binding {
    created: u64,
    device: Option<String>,
    session: String,
    kind: Kind,
}
fn save(binding: Binding) -> Result<(), Fail> {
    use std::hash::{Hash, Hasher};
    let text = serde_json::to_string(&binding).map_err(Fail::new)?;
    let mut hash = std::hash::DefaultHasher::new();
    serde_json::to_string(&(&binding.device, &binding.session, &binding.kind))
        .map_err(Fail::new)?
        .hash(&mut hash);
    let dir = ferry_core::ferry_home()?.join("owned-resources");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{:016x}.json", hash.finish()));
    if !path.exists() {
        ferry_core::write_private(&path, &text)?;
    }
    Ok(())
}
pub fn validate(session: &str) -> Result<(), Fail> {
    if session.is_empty() {
        return Ok(());
    }
    if !ferry_core::session::managed(session) {
        return Err(Fail::new(
            "для автоматической очистки зарегистрируй ferry session или укажи --keep",
        ));
    }
    if !ferry_core::control::ended_sessions(&[session.into()])?.is_empty() {
        return Err(Fail::refused("сессия закончилась"));
    }
    if session.starts_with("agent:") {
        ferry_core::session::ended(&ferry_core::ferry_home()?, session)?;
    }
    Ok(())
}
pub fn job(device: Option<String>, session: &str, request_id: &str) -> Result<(), Fail> {
    validate(session)?;
    if session.is_empty() {
        return Ok(());
    }
    save(Binding {
        created: ferry_core::now(),
        device,
        session: session.into(),
        kind: Kind::Job {
            request_id: request_id.into(),
        },
    })
}
pub fn tunnel(device: String, session: &str, port: u16) -> Result<(), Fail> {
    save(Binding {
        created: ferry_core::now(),
        device: Some(device),
        session: session.into(),
        kind: Kind::Tunnel { port },
    })
}
pub async fn watch(agent: Agent) {
    loop {
        cleanup(&agent).await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}
async fn cleanup(agent: &Agent) {
    let Ok(root) = ferry_core::ferry_home() else {
        return;
    };
    let Ok(entries) = fs::read_dir(root.join("owned-resources")) else {
        return;
    };
    let receipts: Vec<(PathBuf, Binding)> = entries
        .flatten()
        .take(10000)
        .filter_map(|entry| {
            let binding = serde_json::from_slice(&fs::read(entry.path()).ok()?).ok()?;
            Some((entry.path(), binding))
        })
        .collect();
    let names: Vec<_> = receipts
        .iter()
        .map(|(_, b)| b.session.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let Ok(Ok(ended)) =
        tokio::task::spawn_blocking(move || ferry_core::control::ended_sessions(&names)).await
    else {
        return;
    };
    let mut tasks = tokio::task::JoinSet::new();
    for (file, binding) in receipts {
        if !ended.contains(&binding.session) {
            continue;
        }
        while tasks.len() >= 8 {
            tasks.join_next().await;
        }
        let agent = agent.clone();
        tasks.spawn(async move {
            let cleaned = async {
                match binding.kind {
                    Kind::Tunnel { port } => {
                        agent.close_tunnel_owned(
                            binding.device.as_deref().unwrap_or(""),
                            port,
                            &binding.session,
                        )?;
                    }
                    Kind::Job { request_id } => match &binding.device {
                        Some(who) => {
                            agent
                                .device(who)?
                                .execution_cancel_owned(request_id, binding.session.clone())
                                .await?;
                        }
                        None => {
                            agent
                                .execution_cancel_owned_self(request_id, binding.session.clone())
                                .await?;
                        }
                    },
                }
                Ok::<_, Fail>(())
            };
            if matches!(
                tokio::time::timeout(std::time::Duration::from_secs(10), cleaned).await,
                Ok(Ok(()))
            ) {
                let _ = fs::remove_file(file);
            }
        });
    }
    while tasks.join_next().await.is_some() {}
}
