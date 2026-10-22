use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct Project {
    pub root: PathBuf,
    pub name: String,
    pub prefix: String,
    pub snapshot: Option<(String, String)>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct Operation {
    pub id: String,
    pub profile_name: String,
    pub profile: Profile,
    pub cwd: PathBuf,
    pub project: Option<Project>,
    pub isolated: bool,
    pub targets: Vec<(Option<String>, Option<Status>)>,
    pub parallel: usize,
    pub created: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Phase {
    pub name: String,
    pub job: String,
    pub request_id: String,
    pub run_id: Option<String>,
    pub started: u64,
    pub result: Option<Value>,
    #[serde(default)]
    pub finished_seen: Option<u64>,
    #[serde(default)]
    pub ready_log: bool,
    #[serde(default)]
    pub ready_probe: bool,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub carry: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Target {
    pub state: String,
    pub phases: Vec<Phase>,
    pub source: Option<Value>,
    pub synchronized: bool,
    pub lease: bool,
    pub cleanup_pending: bool,
    pub outcome: Option<String>,
    pub deadline: Option<u64>,
    pub row: Value,
}

pub(super) fn valid_id(id: &str) -> Result<(), Fail> {
    if id.is_empty()
        || id.len() > 48
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
    {
        return Err(Fail::new("check id: 1..48 букв, цифр, - и _"));
    }
    Ok(())
}

pub(super) fn root(id: &str) -> Result<PathBuf, Fail> {
    valid_id(id)?;
    Ok(ferry_core::ferry_home()?.join("checks").join(id))
}

pub(super) fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, Fail> {
    if fs::metadata(path)?.len() > 8 * 1024 * 1024 {
        return Err(Fail::new("журнал check больше 8 МиБ"));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(Fail::new)
}

pub(super) fn write<T: Serialize>(path: &Path, value: &T) -> Result<(), Fail> {
    ferry_core::write_private(path, &serde_json::to_string(value).map_err(Fail::new)?)?;
    Ok(())
}

pub(super) fn lock(root: &Path) -> Result<fs::File, Fail> {
    Ok(fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("worker.lock"))?)
}

pub(super) fn active(root: &Path) -> Result<bool, Fail> {
    let file = lock(root)?;
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

pub(super) fn target_path(root: &Path, index: usize) -> PathBuf {
    root.join(format!("target-{index}.json"))
}

pub(super) fn target(root: &Path, op: &Operation, index: usize) -> Result<Target, Fail> {
    let path = target_path(root, index);
    if path.exists() {
        return read(&path);
    }
    let device = op.targets[index].0.as_deref().unwrap_or("local");
    Ok(Target {
        state: "pending".into(),
        row: json!({"device":device,"profile":op.profile_name,"state":"pending","errors":[],"output_directory":format!(".ferry-checks/{}-{index}",op.id)}),
        ..Target::default()
    })
}

pub(super) fn terminal(state: &str) -> bool {
    matches!(state, "succeeded" | "failed" | "cancelled" | "unavailable")
}

pub(super) fn report(root: &Path) -> Result<Value, Fail> {
    let op: Operation = read(&root.join("operation.json"))?;
    let active = active(root)?;
    let cancelled = root.join("cancel").exists();
    let mut rows = Vec::new();
    let mut pending = false;
    for index in 0..op.targets.len() {
        let target = target(root, &op, index)?;
        pending |= !terminal(&target.state) || target.cleanup_pending;
        let mut row = target.row;
        row["state"] = target.state.into();
        row["phases"] = json!(target.phases);
        row["cleanup_pending"] = target.cleanup_pending.into();
        if let Some(source) = target.source {
            row["source"] = source;
        }
        rows.push(row);
    }
    let succeeded = rows.iter().filter(|r| r["state"] == "succeeded").count();
    let state = if pending {
        if active {
            if cancelled { "cancelling" } else { "running" }
        } else {
            "interrupted"
        }
    } else if cancelled {
        "cancelled"
    } else if !rows.is_empty() && succeeded == rows.len() {
        "succeeded"
    } else {
        "failed"
    };
    let worker = read::<Value>(&root.join("worker.json")).ok();
    Ok(
        json!({"id":op.id,"profile":op.profile_name,"state":state,"worker_active":active,"worker":worker,"cancel_requested":cancelled,"succeeded":succeeded,"total":rows.len(),"results":rows,"report_file":root.join("report.json"),"journal":root.join("operation.json")}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stopped_worker_is_interrupted_and_reserves_no_lock() {
        let dir = std::env::temp_dir().join(format!(
            "ferry-check-journal-{}-{}",
            std::process::id(),
            ferry_core::now()
        ));
        fs::create_dir_all(&dir).unwrap();
        let op = Operation {
            id: "test".into(),
            profile_name: "smoke".into(),
            profile: serde_json::from_value(json!({"platforms":{"all":{"command":"true"}}}))
                .unwrap(),
            cwd: dir.clone(),
            project: None,
            isolated: false,
            targets: vec![(None, None)],
            parallel: 1,
            created: 0,
        };
        write(&dir.join("operation.json"), &op).unwrap();
        assert_eq!(report(&dir).unwrap()["state"], "interrupted");
        let guard = lock(&dir).unwrap();
        guard.lock().unwrap();
        assert_eq!(report(&dir).unwrap()["state"], "running");
        drop(guard);
        let mut t = target(&dir, &op, 0).unwrap();
        t.state = "succeeded".into();
        write(&target_path(&dir, 0), &t).unwrap();
        assert_eq!(report(&dir).unwrap()["state"], "succeeded");
        t.cleanup_pending = true;
        write(&target_path(&dir, 0), &t).unwrap();
        assert_eq!(report(&dir).unwrap()["state"], "interrupted");
        fs::remove_dir_all(dir).unwrap();
        assert!(valid_id("../other").is_err());
    }
}
