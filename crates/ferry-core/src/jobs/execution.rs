//! Durable request identity and bounded compute admission. A request never launches twice.
use super::*;
use crate::device::Dir;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const COMPUTE_SLOTS: usize = 2;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Resource {
    Headless,
    #[default]
    Desktop,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub project: String,
    pub commit: String,
    pub tree: String,
    pub workspace: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StartRequest {
    pub request_id: String,
    pub name: String,
    pub command: String,
    pub dir: Dir,
    pub shell: Shell,
    pub resource: Resource,
    pub source: Option<Source>,
    pub session: String,
    pub keep: bool,
    pub timeout_sec: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Started {
    pub request_id: String,
    pub name: String,
    pub run_id: String,
    pub cwd: PathBuf,
    pub reused: bool,
    pub state: String,
}
#[derive(Serialize, Deserialize)]
struct Admission {
    request: StartRequest,
    started: Started,
}

pub fn start(
    request: StartRequest,
    cwd: &Path,
    env: &[(String, String)],
    supervisor: &Path,
) -> io::Result<Started> {
    start_at(&crate::ferry_home()?, request, cwd, env, supervisor)
}
pub fn start_at(
    root: &Path,
    request: StartRequest,
    cwd: &Path,
    env: &[(String, String)],
    supervisor: &Path,
) -> io::Result<Started> {
    validate_name(&request.request_id)?;
    validate_name(&request.name)?;
    if request.request_id.len() > 120
        || request.name.len() > 120
        || request.session.len() > 200
        || request.command.len() > 128 * 1024
    {
        return Err(io::Error::other("слишком длинный запрос запуска"));
    }
    if request
        .timeout_sec
        .is_some_and(|seconds| seconds == 0 || seconds > 86400)
    {
        return Err(io::Error::other("timeout: от 1 до 86400 секунд"));
    }
    shell_with(&request.command, request.shell)?;
    let cwd = fs::canonicalize(cwd)?;
    let requests = root.join("requests");
    fs::create_dir_all(&requests)?;
    let _admission = evidence::guard(root, "execution-admission")?;
    let record = requests.join(format!("{}.json", request.request_id));
    if requests
        .join(format!("{}.cancelled", request.request_id))
        .exists()
    {
        return Err(io::Error::other(
            "request_id отменён; повторный запуск запрещён",
        ));
    }
    if record.exists() {
        let saved: Admission = serde_json::from_slice(&fs::read(&record)?)?;
        if saved.request != request || saved.started.cwd != cwd {
            return Err(io::Error::other(
                "request_id уже использован с другими параметрами",
            ));
        }
        let mut started = saved.started;
        started.reused = true;
        started.state = evidence::locate(root, &started.name, Some(&started.run_id))
            .map(|p| execution_state(&p))
            .unwrap_or_else(|_| "unavailable".into());
        return Ok(started);
    }
    let actual_source = if let Some(source) = &request.source {
        crate::workspace::verify(&cwd, source)?;
        let repo = crate::sync::repo_at(&cwd)?
            .ok_or_else(|| io::Error::other("workspace не является git репозиторием"))?;
        let (commit, tree) = crate::sync::snapshot(&repo)?;
        Some(
            json!({"commit":commit,"tree":tree,"dirty":tree!=source.tree,"captured_at":now(),"includes_ignored_files":false}),
        )
    } else {
        None
    };
    let _job = evidence::guard(root, &request.name)?;
    let dir = root.join("jobs").join(&request.name);
    if running(&dir) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "задача с этим именем уже работает",
        ));
    }
    let jobs = fs::read_dir(root.join("jobs"))
        .into_iter()
        .flatten()
        .flatten();
    let mut compute = 0;
    for entry in jobs {
        let path = entry.path();
        if !running(&path) {
            continue;
        }
        if let Ok(bytes) = fs::read(path.join("execution.json")) {
            let other: StartRequest = serde_json::from_slice(&bytes)?;
            if other.resource == Resource::Headless {
                compute += 1;
            }
            if request
                .source
                .as_ref()
                .zip(other.source.as_ref())
                .is_some_and(|(a, b)| {
                    a.workspace == b.workspace
                        && (request.session.is_empty() || request.session != other.session)
                })
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "рабочая копия занята другой задачей",
                ));
            }
        }
    }
    if request.resource == Resource::Headless && compute >= COMPUTE_SLOTS {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "вычислительные слоты заняты (2); повтори запуск позже",
        ));
    }
    evidence::archive(root, &request.name)?;
    fs::create_dir_all(&dir)?;
    let mut started = Started {
        request_id: request.request_id.clone(),
        name: request.name.clone(),
        run_id: unique_id(),
        cwd: cwd.clone(),
        reused: false,
        state: "starting".into(),
    };
    // Persist the receipt before spawn: an interrupted admission is inspected, never replayed.
    crate::write_private(
        &record,
        &serde_json::to_string(&Admission {
            request: request.clone(),
            started: started.clone(),
        })?,
    )?;
    crate::write_private(
        &dir.join("execution.json"),
        &serde_json::to_string(&request)?,
    )?;
    if let Some(actual) = actual_source {
        crate::write_private(&dir.join("actual-source.json"), &actual.to_string())?;
    }
    let launched = super::launch(
        root,
        &request.name,
        &request.command,
        &cwd,
        env,
        supervisor,
        request.shell,
        &started.run_id,
    );
    match launched {
        Ok(()) => {
            started.state = execution_state(&dir);
        }
        Err(e) => {
            crate::write_private(&dir.join("launch-error"), &e.to_string())?;
            started.state = "lost".into();
        }
    }
    Ok(started)
}

fn execution_state(dir: &Path) -> String {
    if running(dir) {
        "running".into()
    } else {
        code(dir)
    }
}

pub fn lookup(root: &Path, request_id: &str) -> io::Result<Option<Started>> {
    validate_name(request_id)?;
    let _admission = evidence::guard(root, "execution-admission")?;
    let path = root.join("requests").join(format!("{request_id}.json"));
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let saved: Admission = serde_json::from_slice(&bytes)?;
    let mut started = saved.started;
    started.reused = true;
    started.state = evidence::locate(root, &started.name, Some(&started.run_id))
        .map(|p| execution_state(&p))
        .unwrap_or_else(|_| "unavailable".into());
    Ok(Some(started))
}

pub(super) fn event(dir: &Path, event: &str, detail: Value) -> io::Result<()> {
    let mut record = json!({"at":now(),"event":event});
    if let Some(fields) = detail.as_object() {
        for (k, v) in fields {
            record[k] = v.clone();
        }
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("timeline.jsonl"))?;
    writeln!(file, "{record}")
}
pub(super) fn metadata(dir: &Path) -> io::Result<Value> {
    let request: Option<StartRequest> = match fs::read(dir.join("execution.json")) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let shell: Option<Shell> = fs::read(dir.join("shell"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let mut timeline = Vec::new();
    if let Ok(text) = fs::read_to_string(dir.join("timeline.jsonl")) {
        for line in text.lines().take(64) {
            if let Ok(event) = serde_json::from_str::<Value>(line) {
                timeline.push(event);
            }
        }
    }
    let numeric = |name| read(dir, name).and_then(|v| v.parse::<u64>().ok());
    Ok(
        json!({"command":read(dir,"cmd"),"shell":shell,"shell_program":read(dir,"shell-program"),"cwd":read(dir,"cwd"),
        "source":request.as_ref().and_then(|r| r.source.as_ref()),"actual_source":fs::read(dir.join("actual-source.json")).ok().and_then(|bytes|serde_json::from_slice::<Value>(&bytes).ok()),"request_id":request.as_ref().map(|r| &r.request_id),
        "session":request.as_ref().map(|r| &r.session),"resource":request.as_ref().map(|r|r.resource),
        "pid":numeric("child"),"supervisor_pid":numeric("pid"),"started":numeric("started"),"finished":numeric("finished"),
        "keep":request.as_ref().map(|r|r.keep),"timeout_sec":request.as_ref().and_then(|r|r.timeout_sec),"timed_out":dir.join("timed-out").exists(),"state":code(dir),"timeline":timeline,"launch_error":read(dir,"launch-error")}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(id: &str) -> StartRequest {
        StartRequest {
            request_id: id.into(),
            name: format!("job-{id}"),
            command: "exit 0".into(),
            dir: Dir::Home,
            shell: Shell::Default,
            resource: Resource::Headless,
            source: None,
            session: "test-session".into(),
            keep: true,
            timeout_sec: Some(30),
        }
    }
    #[test]
    fn duplicate_requests_never_relaunch_even_after_failed_spawn() {
        let root = std::env::temp_dir().join(format!("ferry-admission-{}", unique_id()));
        fs::create_dir_all(&root).unwrap();
        let request = request("one");
        let missing = root.join("missing-supervisor");
        let first = start_at(&root, request.clone(), &root, &[], &missing).unwrap();
        assert_eq!(first.state, "lost");
        assert!(!first.reused);
        let repeated = start_at(&root, request.clone(), &root, &[], &missing).unwrap();
        assert!(repeated.reused);
        assert_eq!(first.run_id, repeated.run_id);
        let mut conflicting = request;
        conflicting.command = "echo different".into();
        assert!(
            start_at(&root, conflicting, &root, &[], &missing)
                .unwrap_err()
                .to_string()
                .contains("другими параметрами")
        );
        assert_eq!(lookup(&root, "one").unwrap().unwrap().run_id, first.run_id);
        assert!(lookup(&root, "missing").unwrap().is_none());
        let info = metadata(&root.join("jobs/job-one")).unwrap();
        assert_eq!(info["request_id"], "one");
        assert_eq!(info["command"], "exit 0");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn compute_admission_counts_live_locks_and_releases_finished_slots() {
        let root = std::env::temp_dir().join(format!("ferry-slots-{}", unique_id()));
        fs::create_dir_all(&root).unwrap();
        let mut locks = Vec::new();
        for id in ["slot1", "slot2"] {
            let request = request(id);
            let dir = root.join("jobs").join(&request.name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("execution.json"),
                serde_json::to_vec(&request).unwrap(),
            )
            .unwrap();
            let lock = fs::File::create(dir.join("lock")).unwrap();
            lock.lock().unwrap();
            locks.push(lock);
        }
        let third = request("third");
        let missing = root.join("missing");
        assert_eq!(
            start_at(&root, third.clone(), &root, &[], &missing)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(lookup(&root, "third").unwrap().is_none());
        locks.pop();
        assert!(start_at(&root, third, &root, &[], &missing).is_ok());
        drop(locks);
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    #[test]
    fn conflicting_request_id_cannot_stop_another_owner_or_kept_job() {
        let root = std::env::temp_dir().join(format!("ferry-owned-stop-{}", unique_id()));
        let dir = root.join("jobs/owned");
        fs::create_dir_all(&dir).unwrap();
        let mut request = StartRequest {
            request_id: "shared-id".into(),
            name: "owned".into(),
            command: "exit 0".into(),
            dir: Dir::Home,
            shell: Shell::Default,
            resource: Resource::Headless,
            source: None,
            session: "agent:original".into(),
            keep: false,
            timeout_sec: None,
        };
        fs::write(dir.join("run-id"), "run1").unwrap();
        fs::write(dir.join("cmd"), "exit 0").unwrap();
        fs::write(dir.join("log"), "").unwrap();
        fs::write(
            dir.join("execution.json"),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        let lock = fs::File::create(dir.join("lock")).unwrap();
        lock.lock().unwrap();
        let query = |session: &str| evidence::Query {
            name: "owned".into(),
            run_id: Some("run1".into()),
            action: evidence::Action::StopOwned {
                request_id: "shared-id".into(),
                session: session.into(),
            },
        };
        assert_eq!(
            evidence::execute(&root, query("agent:impostor")).unwrap()["stopped"],
            false
        );
        request.keep = true;
        fs::write(
            dir.join("execution.json"),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        assert_eq!(
            evidence::execute(&root, query("agent:original")).unwrap()["stopped"],
            false
        );
        assert!(!dir.join("killed").exists());
        drop(lock);
        fs::remove_dir_all(root).unwrap();
    }
}

/// Cancel an accepted run or reserve a tombstone before a delayed start is admitted.
pub fn cancel(root: &Path, request_id: &str, session: &str) -> io::Result<Value> {
    cancel_inner(root, request_id, session, false)
}
/// Automatic cleanup cannot cancel another owner or a kept run.
pub fn cancel_owned(root: &Path, request_id: &str, session: &str) -> io::Result<Value> {
    cancel_inner(root, request_id, session, true)
}
fn cancel_inner(root: &Path, request_id: &str, session: &str, owned: bool) -> io::Result<Value> {
    validate_name(request_id)?;
    let _admission = evidence::guard(root, "execution-admission")?;
    let folder = root.join("requests");
    fs::create_dir_all(&folder)?;
    let record = folder.join(format!("{request_id}.json"));
    let saved: Option<Admission> = match fs::read(&record) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if owned
        && saved
            .as_ref()
            .is_some_and(|a| a.request.session != session || a.request.keep)
    {
        return Ok(json!({"cancelled":false,"reason":"different-owner-or-kept"}));
    }
    if saved.as_ref().is_some_and(|a| a.request.session != session) {
        return Err(io::Error::other("запрос принадлежит другой сессии"));
    }
    let tombstone = folder.join(format!("{request_id}.cancelled"));
    if owned && tombstone.exists() && fs::read_to_string(&tombstone)? != session {
        return Ok(json!({"cancelled":false,"reason":"different-owner"}));
    }
    if tombstone.exists() && fs::read_to_string(&tombstone)? != session {
        return Err(io::Error::other("отмена принадлежит другой сессии"));
    }
    crate::write_private(&tombstone, session)?;
    if let Some(saved) = saved {
        let _job = evidence::guard(root, &saved.started.name)?;
        if let Ok(dir) = evidence::locate(root, &saved.started.name, Some(&saved.started.run_id))
            && dir == root.join("jobs").join(&saved.started.name)
            && running(&dir)
        {
            super::stop_at(&saved.started.name, &dir)?;
        }
        let mut started = saved.started;
        started.state = evidence::locate(root, &started.name, Some(&started.run_id))
            .map(|p| execution_state(&p))
            .unwrap_or_else(|_| "unavailable".into());
        Ok(json!({"cancelled":true,"started":started}))
    } else {
        Ok(json!({"cancelled":true,"started":null}))
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    #[test]
    fn owner_cancel_preserves_conflicting_or_kept_receipts() {
        let root = std::env::temp_dir().join(format!("ferry-cancel-owner-{}", unique_id()));
        fs::create_dir_all(root.join("requests")).unwrap();
        let mut request = StartRequest {
            request_id: "one".into(),
            name: "one".into(),
            command: "exit 0".into(),
            dir: Dir::Home,
            shell: Shell::Default,
            resource: Resource::Headless,
            source: None,
            session: "agent:one".into(),
            keep: false,
            timeout_sec: None,
        };
        let started = Started {
            request_id: "one".into(),
            name: "one".into(),
            run_id: "run1".into(),
            cwd: root.clone(),
            reused: false,
            state: "unavailable".into(),
        };
        let save = |r: &StartRequest| {
            fs::write(
                root.join("requests/one.json"),
                serde_json::to_vec(&Admission {
                    request: r.clone(),
                    started: started.clone(),
                })
                .unwrap(),
            )
            .unwrap()
        };
        save(&request);
        assert_eq!(
            cancel_owned(&root, "one", "agent:two").unwrap()["cancelled"],
            false
        );
        request.keep = true;
        save(&request);
        assert_eq!(
            cancel_owned(&root, "one", "agent:one").unwrap()["cancelled"],
            false
        );
        assert!(!root.join("requests/one.cancelled").exists());
        assert_eq!(
            cancel_owned(&root, "delayed", "agent:one").unwrap()["cancelled"],
            true
        );
        request.request_id = "delayed".into();
        request.keep = false;
        assert!(
            start_at(&root, request, &root, &[], &root.join("missing"))
                .unwrap_err()
                .to_string()
                .contains("отменён")
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancellation_before_admission_prevents_late_start() {
        let root = std::env::temp_dir().join(format!("ferry-cancel-{}", unique_id()));
        fs::create_dir_all(&root).unwrap();
        assert_eq!(
            cancel(&root, "delayed", "agent:one").unwrap()["cancelled"],
            true
        );
        let request = StartRequest {
            request_id: "delayed".into(),
            name: "late".into(),
            command: "exit 0".into(),
            dir: Dir::Home,
            shell: Shell::Default,
            resource: Resource::Headless,
            source: None,
            session: "agent:one".into(),
            keep: true,
            timeout_sec: None,
        };
        assert!(
            start_at(&root, request, &root, &[], &root.join("missing"))
                .unwrap_err()
                .to_string()
                .contains("отменён")
        );
        assert!(!root.join("jobs/late").exists());
        assert!(cancel(&root, "delayed", "agent:other").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
