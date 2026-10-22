use super::{Item, checks, store};
use ferry_core::{
    control::{self, Request, Response},
    device::{Fail, Snapshot},
    mode::Mode,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};
use store::Candidate;

fn ask(request: Request) -> Result<Response, Fail> {
    match control::call_session_timeout(request, "queue", Duration::from_secs(5))? {
        Response::Error(error) => Err(error),
        value => Ok(value),
    }
}
fn state() -> Result<Snapshot, Fail> {
    match ask(Request::State)? {
        Response::State(value) => Ok(value),
        _ => Err(Fail::new("queue: неожиданный ответ state")),
    }
}

pub(super) fn spawn(root: &Path) -> Result<(), Fail> {
    if store::active(root)? {
        return Ok(());
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("worker.log"))?;
    let exe = ferry_core::jobs::prepare_supervisor(
        &ferry_core::ferry_home()?,
        &std::env::current_exe()?,
    )?;
    let mut command = Command::new(exe);
    command
        .args(["queue", "worker"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x08000000 | 0x00000200);
    let mut child = command.spawn()?;
    for _ in 0..100 {
        if store::active(root)? || child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(Fail::new(
        "queue worker не подтвердил запуск; status/resume покажет состояние",
    ))
}

fn candidates(item: &Item, snapshot: &Snapshot) -> Result<(Vec<Candidate>, Vec<Value>), Fail> {
    let mut values = Vec::new();
    let mut reasons = Vec::new();
    for selector in item
        .selector
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if selector == "local" {
            if !values.iter().any(|c: &Candidate| c.device.is_none()) {
                values.push(Candidate {
                    device: None,
                    key: snapshot.id.clone(),
                    label: snapshot.me.name.clone(),
                    status: snapshot.me.clone(),
                });
            }
            continue;
        }
        let peers: Vec<_> = match selector {
            "all" => snapshot.peers.iter().collect(),
            os if os.starts_with("os:") => snapshot
                .peers
                .iter()
                .filter(|p| p.status.as_ref().is_some_and(|s| s.os == os[3..]))
                .collect(),
            name => match snapshot.find(name) {
                Ok(peer) => vec![peer],
                Err(error) => {
                    reasons.push(json!({"device":name,"reason":error}));
                    continue;
                }
            },
        };
        for peer in peers {
            if !peer.online || peer.status.is_none() {
                reasons.push(json!({"device":peer.name,"reason":"устройство не на связи"}));
            } else if !values.iter().any(|c| c.key == peer.id) {
                values.push(Candidate {
                    device: Some(peer.id.clone()),
                    key: peer.id.clone(),
                    label: peer.name.clone(),
                    status: peer.status.clone().unwrap(),
                });
            }
        }
    }
    if values.len() > 64 {
        return Err(Fail::new("queue: не более 64 кандидатов"));
    }
    Ok((values, reasons))
}

fn requirements<'a>(item: &'a Item, candidate: &Candidate) -> Result<(&'a Value, bool), Fail> {
    let status = &candidate.status;
    if !status.role.accepts_commands() || status.mode == Mode::Paused {
        return Err(Fail::new("режим или роль запрещают команды"));
    }
    let version: Vec<_> = status
        .version
        .split('.')
        .filter_map(|v| v.parse::<u32>().ok())
        .collect();
    if version.len() != 3 || version.as_slice() < [0, 1, 23].as_slice() {
        return Err(Fail::new("нужен ferry 0.1.23 или новее"));
    }
    if item.os.as_ref().is_some_and(|os| os != &status.os) {
        return Err(Fail::new("другая операционная система"));
    }
    let desktop = item.plan["profile"]["headless"] != true;
    if desktop && !status.mode.allows_desktop() {
        return Err(Fail::new("рабочий стол запрещён режимом"));
    }
    let specs = &item.plan["profile"]["platforms"];
    let spec = specs
        .get(&status.os)
        .or_else(|| specs.get("all"))
        .ok_or_else(|| Fail::new("нет команды профиля для этой системы"))?;
    Ok((spec, desktop))
}
fn eligible(item: &Item, candidate: &Candidate, reserved: &[String]) -> Result<(), Fail> {
    let (spec, desktop) = requirements(item, candidate)?;
    if reserved.contains(&candidate.key) {
        return Err(Fail::new("устройство уже занято операцией очереди"));
    }
    let request = ferry_core::capabilities::Request {
        shell: serde_json::from_value(spec["shell"].clone()).map_err(Fail::new)?,
        tools: serde_json::from_value(spec["tools"].clone()).map_err(Fail::new)?,
        desktop,
    };
    let device = candidate.device.as_deref().unwrap_or("local");
    let capabilities = if candidate.device.is_none() {
        ferry_core::capabilities::collect(request)?
    } else {
        match ask(Request::Capabilities {
            device: device.into(),
            request,
        })? {
            Response::Capabilities(value) => value,
            _ => return Err(Fail::new("неожиданный ответ capabilities")),
        }
    };
    if !capabilities.shell_available
        || !capabilities.missing_tools.is_empty()
        || !capabilities.warnings.is_empty()
    {
        return Err(Fail::new(format!(
            "shell/tools: {}",
            serde_json::to_string(&capabilities).map_err(Fail::new)?
        )));
    }
    let rows = if candidate.device.is_none() {
        ferry_core::jobs::rows()?
    } else {
        match ask(Request::Jobs {
            device: device.into(),
        })? {
            Response::Jobs(rows) => rows,
            _ => return Err(Fail::new("неожиданный ответ jobs")),
        }
    };
    let running = rows.iter().filter(|job| job.running()).count();
    // Conservatively count legacy desktop tasks too: never promise a free compute slot from incomplete metadata.
    let needed = if !desktop && spec["launch"].is_string() {
        2
    } else {
        1
    };
    if running + needed > 2 {
        return Err(Fail::new("недостаточно свободных слотов задач"));
    }
    if desktop {
        if capabilities.locked != Some(false) {
            return Err(Fail::new(
                "рабочий стол заблокирован или состояние неизвестно",
            ));
        }
        match ask(Request::Desk {
            device: device.into(),
            action: "show".into(),
            minutes: 0,
        })? {
            Response::Message(text) if text.trim() == "свободен" => {}
            _ => return Err(Fail::new("рабочий стол занят или бронь неизвестна")),
        }
        let presence = match ask(Request::Human {
            device: candidate.device.clone(),
            request: ferry_core::human::Request::Presence,
        })? {
            Response::Report(value) => serde_json::from_str::<Value>(&value).map_err(Fail::new)?,
            _ => return Err(Fail::new("нет сигнала присутствия человека")),
        };
        if presence["available"] != true || presence["idle_seconds"].as_u64().is_none_or(|n| n < 30)
        {
            return Err(Fail::new(
                "нужно 30 секунд без физического ввода; неизвестный сигнал означает ожидание",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Next {
    Observe,
    Start,
    Cancel,
    Interrupted,
    Complete(String),
}
fn next(item: &Item, report: Option<&Value>) -> Next {
    if report.is_some_and(|r| r["worker_active"] == true) {
        return Next::Observe;
    }
    if let Some(state) = report.and_then(|r| r["state"].as_str())
        && matches!(state, "succeeded" | "failed" | "cancelled")
    {
        return Next::Complete(state.into());
    }
    if item.cancel_requested {
        if !item.start_attempted {
            return Next::Complete("cancelled".into());
        }
        return if !item.cancel_attempted || item.resume_requested {
            Next::Cancel
        } else {
            Next::Interrupted
        };
    }
    if !item.start_attempted || item.resume_requested {
        Next::Start
    } else {
        Next::Interrupted
    }
}
fn update_current(
    root: &Path,
    id: &str,
    revision: u64,
    change: impl FnOnce(&mut Item),
) -> Result<bool, Fail> {
    let mut applied = false;
    store::edit(root, id, |item| {
        if item.revision == revision {
            change(item);
            applied = true;
        }
    })?;
    Ok(applied)
}
fn interrupted(
    root: &Path,
    id: &str,
    revision: u64,
    error: impl std::fmt::Display,
) -> Result<(), Fail> {
    update_current(root, id, revision, |item| {
        item.state = "interrupted".into();
        item.resume_requested = false;
        if item.cancel_requested {
            item.cancel_attempted = true;
        }
        item.reasons = json!([{"reason":error.to_string()}]);
    })?;
    Ok(())
}
fn assigned(root: &Path, id: &str) -> Result<(), Fail> {
    let item = store::read(root, id)?;
    let report = if item.start_attempted {
        match checks::queue_status(&item.operation_id) {
            Ok(r) => Some(r),
            Err(error) => {
                interrupted(root, id, item.revision, error)?;
                return Ok(());
            }
        }
    } else {
        None
    };
    let decision = next(&item, report.as_ref());
    if let Some(report) = report {
        store::edit(root, id, |current| {
            current.check = Some(
                json!({"id":report["id"],"state":report["state"],"worker_active":report["worker_active"],"report_file":report["report_file"]}),
            )
        })?;
    }
    match decision {
        Next::Complete(state) => {
            update_current(root, id, item.revision, |item| {
                item.state = state;
                item.resume_requested = false;
            })?;
        }
        Next::Observe => {
            // Cancellation is safe while the check worker is alive; its marker is durable.
            if item.cancel_requested && (!item.cancel_attempted || item.resume_requested) {
                let apply = update_current(root, id, item.revision, |i| {
                    i.cancel_attempted = true;
                    i.resume_requested = false;
                    i.state = "cancelling".into();
                })?;
                if apply && let Err(error) = checks::queue_control(&item.operation_id, true) {
                    interrupted(root, id, item.revision, error)?;
                }
            } else {
                update_current(root, id, item.revision, |i| {
                    i.state = if i.cancel_requested {
                        "cancelling"
                    } else {
                        "running"
                    }
                    .into();
                    i.resume_requested = false;
                })?;
            }
        }
        Next::Interrupted => {
            interrupted(
                root,
                id,
                item.revision,
                "запуск уже запрашивался; resume продолжает тот же operation_id, cancel завершает его",
            )?;
        }
        Next::Cancel => {
            let apply = update_current(root, id, item.revision, |i| {
                i.cancel_attempted = true;
                i.resume_requested = false;
                i.state = "cancelling".into();
            })?;
            if apply && let Err(error) = checks::queue_control(&item.operation_id, true) {
                interrupted(root, id, item.revision, error)?;
            }
        }
        Next::Start => {
            let candidate = item
                .assigned
                .as_ref()
                .ok_or_else(|| Fail::new("нет закреплённого устройства"))?;
            if let Err(error) = checks::queue_admit(
                &item.plan,
                candidate.device.clone(),
                candidate.status.clone(),
            ) {
                interrupted(root, id, item.revision, error)?;
                return Ok(());
            }
            let mut launch = false;
            update_current(root, id, item.revision, |i| {
                if !i.cancel_requested && (!i.start_attempted || i.resume_requested) {
                    i.start_attempted = true;
                    i.resume_requested = false;
                    i.state = "dispatching".into();
                    launch = true;
                }
            })?;
            if launch && let Err(error) = checks::queue_control(&item.operation_id, false) {
                interrupted(root, id, item.revision, error)?;
            }
        }
    }
    Ok(())
}
fn waiting(root: &Path, item: &Item, reserved: &[String]) -> Result<(), Fail> {
    let snapshot = match state() {
        Ok(value) => value,
        Err(error) => {
            store::edit(root, &item.id, |i| {
                i.reasons = json!([{"reason":error.to_string()}])
            })?;
            return Ok(());
        }
    };
    let (candidates, mut reasons) = candidates(item, &snapshot)?;
    if candidates.is_empty() {
        reasons.push(json!({"reason":"нет доступных кандидатов"}));
    }
    // One bounded probe per pass keeps cancellation and active check observation responsive.
    if let Some(candidate) = candidates.get(item.candidate_offset % candidates.len().max(1)) {
        match eligible(item, candidate, reserved) {
            Ok(()) => {
                store::edit(root, &item.id, |i| {
                    if i.assigned.is_none() && !i.cancel_requested && ferry_core::now() < i.deadline
                    {
                        i.assigned = Some(candidate.clone());
                        i.state = "assigned".into();
                        i.reasons = json!([]);
                    }
                })?;
                return Ok(());
            }
            Err(error) => {
                reasons.push(json!({"device":candidate.label,"reason":error.to_string()}))
            }
        }
    }
    store::edit(root, &item.id, |i| {
        i.reasons = Value::Array(reasons);
        i.candidate_offset = i.candidate_offset.wrapping_add(1);
    })?;
    Ok(())
}
pub(super) fn run(root: &Path) -> Result<i32, Fail> {
    let lock = store::lock(root, "worker.lock")?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(0),
        Err(fs::TryLockError::Error(e)) => return Err(e.into()),
    }
    let mut cursor = 0usize;
    loop {
        let items = store::list(root)?;
        for item in items.iter().filter(|i| !store::terminal(&i.state)) {
            if item.assigned.is_some() {
                assigned(root, &item.id)?;
            } else if item.cancel_requested || ferry_core::now() >= item.deadline {
                store::edit(root, &item.id, |i| {
                    i.state = if i.cancel_requested {
                        "cancelled"
                    } else {
                        "expired"
                    }
                    .into()
                })?;
            }
        }
        let items = store::list(root)?;
        let reserved: Vec<_> = items
            .iter()
            .filter(|i| !store::terminal(&i.state))
            .filter_map(|i| i.assigned.as_ref().map(|c| c.key.clone()))
            .collect();
        let pending: Vec<_> = items
            .iter()
            .filter(|i| !store::terminal(&i.state) && i.assigned.is_none())
            .collect();
        if reserved.len() < 2 && !pending.is_empty() {
            waiting(root, pending[cursor % pending.len()], &reserved)?;
            cursor = cursor.wrapping_add(1);
        }
        let state_guard = store::lock(root, "state.lock")?;
        state_guard.lock()?;
        let actionable = store::list(root)?.iter().any(|i| {
            !store::terminal(&i.state)
                && (i.state != "interrupted"
                    || i.resume_requested
                    || (i.cancel_requested && !i.cancel_attempted))
        });
        if !actionable {
            drop(lock);
            return Ok(0);
        }
        drop(state_guard);
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_observation_and_cancel_cannot_consume_a_new_control_request() {
        let root =
            std::env::temp_dir().join(format!("ferry-queue-{}", ferry_core::jobs::unique_id()));
        fs::create_dir_all(root.join("items")).unwrap();
        let mut item = store::tests::fixture();
        item.revision = 2;
        item.resume_requested = true;
        item.cancel_requested = true;
        store::write(&root, &item).unwrap();
        let applied = update_current(&root, &item.id, 1, |current| {
            current.resume_requested = false;
            current.cancel_attempted = true;
            current.state = "running".into();
        })
        .unwrap();
        assert!(!applied, "stale worker must not issue its control call");
        let saved = store::read(&root, &item.id).unwrap();
        assert!(saved.resume_requested);
        assert!(!saved.cancel_attempted);
        assert_eq!(saved.state, "waiting");
        assert!(
            update_current(&root, &item.id, 2, |current| current.resume_requested =
                false)
            .unwrap()
        );
        assert!(!store::read(&root, &item.id).unwrap().resume_requested);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn stale_worker_result_cannot_erase_a_new_resume_request() {
        let root =
            std::env::temp_dir().join(format!("ferry-queue-{}", ferry_core::jobs::unique_id()));
        fs::create_dir_all(root.join("items")).unwrap();
        let mut item = store::tests::fixture();
        item.revision = 2;
        item.resume_requested = true;
        store::write(&root, &item).unwrap();
        interrupted(&root, &item.id, 1, "old failure").unwrap();
        let saved = store::read(&root, &item.id).unwrap();
        assert!(saved.resume_requested);
        assert_eq!(saved.state, "waiting");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancelled_and_expired_requests_finish_without_discovery_or_dispatch() {
        let root =
            std::env::temp_dir().join(format!("ferry-queue-{}", ferry_core::jobs::unique_id()));
        fs::create_dir_all(root.join("items")).unwrap();
        let mut expired = store::tests::fixture();
        expired.deadline = 0;
        store::write(&root, &expired).unwrap();
        let mut cancelled = expired.clone();
        cancelled.id = "two".into();
        cancelled.operation_id = "q-two".into();
        cancelled.deadline = u64::MAX;
        cancelled.cancel_requested = true;
        store::write(&root, &cancelled).unwrap();
        assert_eq!(run(&root).unwrap(), 0);
        let expired = store::read(&root, "one").unwrap();
        let cancelled = store::read(&root, "two").unwrap();
        assert_eq!(expired.state, "expired");
        assert_eq!(cancelled.state, "cancelled");
        assert!(!expired.start_attempted && !cancelled.start_attempted);
        assert!(expired.assigned.is_none() && cancelled.assigned.is_none());
        assert!(!store::active(&root).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn one_worker_owns_the_queue_and_other_worker_cannot_dispatch() {
        let root =
            std::env::temp_dir().join(format!("ferry-queue-{}", ferry_core::jobs::unique_id()));
        fs::create_dir_all(root.join("items")).unwrap();
        let item = store::tests::fixture();
        store::write(&root, &item).unwrap();
        let lock = store::lock(&root, "worker.lock").unwrap();
        lock.lock().unwrap();
        assert_eq!(run(&root).unwrap(), 0);
        assert_eq!(store::read(&root, "one").unwrap().state, "waiting");
        drop(lock);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn ambiguous_dispatch_is_never_retried_without_explicit_resume() {
        let mut item = store::tests::fixture();
        assert_eq!(next(&item, None), Next::Start);
        item.start_attempted = true;
        assert_eq!(next(&item, None), Next::Interrupted);
        item.resume_requested = true;
        assert_eq!(next(&item, None), Next::Start);
        item.cancel_requested = true;
        assert_eq!(next(&item, None), Next::Cancel);
        item.cancel_attempted = true;
        item.resume_requested = false;
        assert_eq!(next(&item, None), Next::Interrupted);
        assert_eq!(
            next(
                &item,
                Some(&json!({"state":"succeeded","worker_active":false}))
            ),
            Next::Complete("succeeded".into())
        );
    }
    #[test]
    fn cancel_before_dispatch_cannot_start_a_check() {
        let mut item = store::tests::fixture();
        item.cancel_requested = true;
        assert_eq!(next(&item, None), Next::Complete("cancelled".into()));
    }
    #[test]
    fn desktop_and_platform_requirements_are_conservative() {
        let mut item = store::tests::fixture();
        let mut candidate = Candidate {
            device: None,
            label: "local".into(),
            key: "one".into(),
            status: ferry_core::device::Status {
                name: "local".into(),
                os: "macos".into(),
                version: "0.1.24".into(),
                mode: Mode::NoDesktop,
                role: ferry_core::device::Role::Both,
            },
        };
        assert!(requirements(&item, &candidate).is_ok());
        item.plan["profile"]["headless"] = false.into();
        assert!(requirements(&item, &candidate).is_err());
        candidate.status.mode = Mode::Active;
        assert!(requirements(&item, &candidate).is_ok());
        item.os = Some("windows".into());
        assert!(requirements(&item, &candidate).is_err());
    }
}
