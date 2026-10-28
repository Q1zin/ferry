use super::*;
use std::{fs, path::Path};

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub(super) struct Candidate {
    pub device: Option<String>,
    pub label: String,
    pub key: String,
    pub status: ferry_core::device::Status,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Item {
    pub id: String,
    pub operation_id: String,
    pub created: u64,
    pub deadline: u64,
    pub wait_seconds: u32,
    pub selector: String,
    pub os: Option<String>,
    pub plan: Value,
    pub state: String,
    pub assigned: Option<Candidate>,
    pub start_attempted: bool,
    pub cancel_requested: bool,
    #[serde(default)]
    pub cancel_attempted: bool,
    pub resume_requested: bool,
    #[serde(default)]
    pub revision: u64,
    pub reasons: Value,
    pub check: Option<Value>,
    pub candidate_offset: usize,
}
pub(super) fn valid(id: &str) -> Result<(), Fail> {
    if id.is_empty()
        || id.len() > 40
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(Fail::new("queue id: 1..40 букв ASCII, цифр, - и _"));
    }
    Ok(())
}
pub(super) fn root() -> Result<PathBuf, Fail> {
    let root = ferry_core::ferry_home()?.join("queue");
    fs::create_dir_all(root.join("items"))?;
    Ok(root)
}
pub(super) fn path(root: &Path, id: &str) -> Result<PathBuf, Fail> {
    valid(id)?;
    Ok(root.join("items").join(format!("{id}.json")))
}
pub(super) fn read(root: &Path, id: &str) -> Result<Item, Fail> {
    let path = path(root, id)?;
    if fs::metadata(&path)?.len() > 256 * 1024 {
        return Err(Fail::new("queue journal больше 256 КиБ"));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(Fail::new)
}
pub(super) fn write(root: &Path, item: &Item) -> Result<(), Fail> {
    let value = serde_json::to_string(item).map_err(Fail::new)?;
    if value.len() > 256 * 1024 {
        return Err(Fail::new("queue journal больше 256 КиБ"));
    }
    ferry_core::write_private(&path(root, &item.id)?, &value)?;
    Ok(())
}
pub(super) fn lock(root: &Path, name: &str) -> Result<fs::File, Fail> {
    Ok(fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(name))?)
}
pub(super) fn edit(root: &Path, id: &str, change: impl FnOnce(&mut Item)) -> Result<Item, Fail> {
    let guard = lock(root, "state.lock")?;
    guard.lock()?;
    let mut item = read(root, id)?;
    change(&mut item);
    write(root, &item)?;
    Ok(item)
}
pub(super) fn list(root: &Path) -> Result<Vec<Item>, Fail> {
    let mut items = Vec::new();
    for entry in fs::read_dir(root.join("items"))?.take(1001) {
        if items.len() == 1000 {
            return Err(Fail::new(
                "queue: более 1000 записей; архивируй завершённые журналы вручную",
            ));
        }
        let entry = entry?;
        if entry.path().extension().is_some_and(|s| s == "json") {
            let id = entry
                .path()
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            items.push(read(root, &id)?);
        }
    }
    items.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
    Ok(items)
}
pub(super) fn terminal(state: &str) -> bool {
    matches!(state, "succeeded" | "failed" | "cancelled" | "expired")
}
pub(super) fn active(root: &Path) -> Result<bool, Fail> {
    let guard = lock(root, "worker.lock")?;
    match guard.try_lock() {
        Ok(()) => Ok(false),
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(e)) => Err(e.into()),
    }
}
pub(super) fn report(item: &Item, worker: bool) -> Value {
    json!({"id":item.id,"operation_id":item.operation_id,"state":if !worker && !terminal(&item.state) && item.state!="interrupted"{"interrupted"}else{&item.state},
    "saved_state":item.state,"worker_active":worker,"created":item.created,"wait_deadline":item.deadline,"profile":item.plan["profile_name"],
    "requirements":{"devices":item.selector,"os":item.os,"headless":item.plan["profile"]["headless"]},
    "assigned":item.assigned,"start_attempted":item.start_attempted,"cancel_requested":item.cancel_requested,"reasons":item.reasons,"check":item.check})
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    #[test]
    fn interrupted_assignment_never_becomes_waiting() {
        let mut item = fixture();
        item.assigned = Some(Candidate {
            device: None,
            label: "local".into(),
            key: "one".into(),
            status: ferry_core::device::Status {
                name: "local".into(),
                os: "macos".into(),
                version: "0.1.24".into(),
                mode: ferry_core::mode::Mode::Active,
                role: ferry_core::device::Role::Both,
            },
        });
        item.start_attempted = true;
        item.state = "running".into();
        let value = report(&item, false);
        assert_eq!(value["state"], "interrupted");
        assert_eq!(value["assigned"]["key"], "one");
        assert_eq!(value["start_attempted"], true);
    }
    pub(in crate::queue) fn fixture() -> Item {
        Item {
            id: "one".into(),
            operation_id: "q-one".into(),
            created: 1,
            deadline: 60,
            wait_seconds: 60,
            selector: "all".into(),
            os: None,
            plan: json!({"profile_name":"smoke","profile":{"headless":true,"platforms":{"all":{"command":"true","tools":[],"shell":"default"}}}}),
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
        }
    }
}
