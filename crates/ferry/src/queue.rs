//! A local durable admission queue. Already-dispatched operations never migrate or retry implicitly.
use crate::{checks, devices::finish};
use ferry_core::device::Fail;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io, path::PathBuf};
mod store;
mod worker;
use store::Item;

fn new_id() -> String {
    format!(
        "q-{:x}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}
fn response(root: &std::path::Path, id: &str) -> Result<i32, Fail> {
    let item = store::read(root, id)?;
    let report = store::report(&item, store::active(root)?);
    println!("{report}");
    Ok(i32::from(matches!(
        report["state"].as_str(),
        Some("failed" | "expired" | "interrupted")
    )))
}
fn equivalent(a: &Item, b: &Item) -> bool {
    fn plan(mut value: Value) -> Value {
        value["created"] = Value::Null;
        if let Some(array) = value
            .get_mut("project")
            .and_then(|p| p.get_mut("snapshot"))
            .and_then(Value::as_array_mut)
            && !array.is_empty()
        {
            array[0] = Value::Null;
        }
        value
    }
    a.selector == b.selector
        && a.os == b.os
        && a.wait_seconds == b.wait_seconds
        && plan(a.plan.clone()) == plan(b.plan.clone())
}
pub fn run(args: &[&str]) -> io::Result<i32> {
    finish((|| {
        let root = store::root()?;
        match args {
            ["worker"] => worker::run(&root),
            ["list"] => {
                let active = store::active(&root)?;
                let rows: Vec<_> = store::list(&root)?
                    .iter()
                    .map(|i| store::report(i, active))
                    .collect();
                println!("{}", json!({"worker_active":active,"items":rows}));
                Ok(0)
            }
            ["status", id] => response(&root, id),
            [action @ ("resume" | "cancel"), id] => {
                let current = store::read(&root, id)?;
                if store::terminal(&current.state) {
                    return response(&root, id);
                }
                store::edit(&root, id, |item| {
                    item.revision = item.revision.wrapping_add(1);
                    if *action == "cancel" {
                        item.cancel_requested = true;
                        item.cancel_attempted = false;
                    } else {
                        item.resume_requested = true;
                        if item.assigned.is_none() {
                            item.state = "waiting".into();
                        }
                    }
                })?;
                worker::spawn(&root)?;
                response(&root, id)
            }
            ["restart", id] => {
                let mut item = store::read(&root, id)?;
                if !store::terminal(&item.state) {
                    return Err(Fail::new(
                        "restart требует завершённую очередь; для неопределённого запуска resume/cancel с прежним id",
                    ));
                }
                item.id = new_id();
                item.operation_id = format!("q-{}", item.id);
                item.plan["id"] = item.operation_id.clone().into();
                item.created = ferry_core::now();
                item.plan["created"] = item.created.into();
                item.deadline = item.created + u64::from(item.wait_seconds);
                item.state = "waiting".into();
                item.assigned = None;
                item.start_attempted = false;
                item.cancel_requested = false;
                item.cancel_attempted = false;
                item.resume_requested = false;
                item.revision = 0;
                item.check = None;
                item.reasons = json!([]);
                item.candidate_offset = 0;
                let guard = store::lock(&root, "state.lock")?;
                guard.lock()?;
                if store::list(&root)?.len() >= 1000 {
                    return Err(Fail::new("очередь содержит 1000 записей"));
                }
                store::write(&root, &item)?;
                drop(guard);
                worker::spawn(&root)?;
                response(&root, &item.id)
            }
            ["add", profile, tail @ ..] => {
                let (
                    mut file,
                    mut selector,
                    mut os,
                    mut wait,
                    mut id,
                    mut home,
                    mut no_sync,
                    mut isolated,
                ) = (
                    PathBuf::from("ferry.checks.json"),
                    "all".to_string(),
                    None,
                    600u32,
                    new_id(),
                    false,
                    false,
                    false,
                );
                let mut rest = tail;
                while !rest.is_empty() {
                    match rest {
                        ["--file", v, r @ ..] => {
                            file = PathBuf::from(v);
                            rest = r;
                        }
                        ["--devices", v, r @ ..] => {
                            selector = (*v).into();
                            rest = r;
                        }
                        ["--os", v @ ("macos" | "windows"), r @ ..] => {
                            os = Some((*v).to_string());
                            rest = r;
                        }
                        ["--wait", v, r @ ..] => {
                            wait = v.parse().map_err(Fail::new)?;
                            rest = r;
                        }
                        ["--id", v, r @ ..] => {
                            id = (*v).into();
                            rest = r;
                        }
                        ["--home", r @ ..] => {
                            home = true;
                            rest = r;
                        }
                        ["--no-sync", r @ ..] => {
                            no_sync = true;
                            rest = r;
                        }
                        ["--isolated", r @ ..] => {
                            isolated = true;
                            rest = r;
                        }
                        _ => {
                            return Err(Fail::new(
                                "queue add PROFILE [--file PATH] [--devices SELECTOR] [--os macos|windows] [--wait 1..86400] [--id ID] [--home|--no-sync|--isolated]",
                            ));
                        }
                    }
                }
                store::valid(&id)?;
                if !(1..=86400).contains(&wait) || selector.is_empty() || selector.len() > 4096 {
                    return Err(Fail::new(
                        "queue: wait 1..86400 и непустой selector до 4096 байт",
                    ));
                }
                let operation_id = format!("q-{id}");
                let plan =
                    checks::queue_prepare(profile, &file, home, no_sync, isolated, &operation_id)?;
                let now = ferry_core::now();
                let item = Item {
                    id: id.clone(),
                    operation_id,
                    created: now,
                    deadline: now + u64::from(wait),
                    wait_seconds: wait,
                    selector,
                    os,
                    plan,
                    state: "waiting".into(),
                    assigned: None,
                    start_attempted: false,
                    cancel_requested: false,
                    cancel_attempted: false,
                    resume_requested: false,
                    revision: 0,
                    reasons: json!([]),
                    check: None,
                    candidate_offset: 0,
                };
                let guard = store::lock(&root, "state.lock")?;
                guard.lock()?;
                if store::path(&root, &id)?.exists() {
                    let old = store::read(&root, &id)?;
                    if !equivalent(&old, &item) {
                        return Err(Fail::new(
                            "queue id уже связан с другими параметрами; resume продолжает прежний запрос",
                        ));
                    }
                } else {
                    if store::list(&root)?.len() >= 1000 {
                        return Err(Fail::new("очередь содержит 1000 записей"));
                    }
                    store::write(&root, &item)?;
                }
                drop(guard);
                worker::spawn(&root)?;
                response(&root, &id)
            }
            _ => Err(Fail::new(
                "queue add|list|status ID|cancel ID|resume ID|restart ID",
            )),
        }
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_id_compares_tree_and_configuration_not_snapshot_commit_time() {
        let mut old = store::tests::fixture();
        old.plan["project"] = json!({"root":"repo","snapshot":["commit-a","tree-a"]});
        let mut new = old.clone();
        new.plan["created"] = 100.into();
        old.plan["created"] = 1.into();
        new.plan["project"]["snapshot"][0] = "commit-b".into();
        assert!(equivalent(&old, &new));
        new.plan["project"]["snapshot"][1] = "tree-b".into();
        assert!(!equivalent(&old, &new));
        new = old.clone();
        new.selector = "other-device".into();
        assert!(!equivalent(&old, &new));
    }
}
