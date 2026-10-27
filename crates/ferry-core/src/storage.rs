//! Logical disk use and explicit cleanup of generated data. Source and run receipts stay.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Shots,
    Observations,
    Recordings,
    Checks,
    Diagnostics,
    Outputs,
    Caches,
    Runners,
    Questions,
    Projects,
    Workspaces,
    Jobs,
    Archives,
}
impl Kind {
    fn removable(self) -> bool {
        !matches!(
            self,
            Self::Projects | Self::Workspaces | Self::Jobs | Self::Archives
        )
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub kind: Option<Kind>,
    pub only: Option<String>,
    pub older_than_days: Option<u32>,
    pub max_bytes: Option<u64>,
    pub apply: Option<String>,
}
#[derive(Serialize)]
struct Unit {
    id: String,
    kind: Kind,
    path: PathBuf,
    bytes: u64,
    files: u64,
    modified: u64,
    fingerprint: String,
    protected: Option<String>,
}
#[derive(Default)]
struct Measure {
    bytes: u64,
    files: u64,
    modified: u64,
    hash: DefaultHasher,
}
struct Scan {
    entries: usize,
    started: Instant,
}
impl Scan {
    fn tick(&mut self, depth: usize) -> io::Result<()> {
        self.entries += 1;
        if self.entries > 1_000_000 || depth > 64 || self.started.elapsed().as_secs() > 30 {
            return Err(io::Error::other(
                "storage: предел сканирования (1 млн файлов / 64 уровня / 30 с), ничего не удалено",
            ));
        }
        Ok(())
    }
    fn measure(&mut self, path: &Path, m: &mut Measure, depth: usize) -> io::Result<()> {
        self.tick(depth)?;
        let metadata = fs::symlink_metadata(path)?;
        path.hash(&mut m.hash);
        metadata.len().hash(&mut m.hash);
        metadata.modified()?.hash(&mut m.hash);
        m.modified = m.modified.max(seconds(metadata.modified()?));
        if metadata.is_dir() {
            for child in children(path)? {
                self.measure(&child, m, depth + 1)?;
            }
        } else {
            m.bytes = m.bytes.saturating_add(metadata.len());
            m.files += 1;
        }
        Ok(())
    }
}
fn seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn children(path: &Path) -> io::Result<Vec<PathBuf>> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink()) {
        return Ok(Vec::new());
    }
    let mut paths = match fs::read_dir(path) {
        Ok(entries) => entries
            .map(|entry| entry.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    paths.sort();
    Ok(paths)
}
fn json_file(path: &Path) -> io::Result<Value> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > 8 * 1024 * 1024 {
        return Err(io::Error::other("небезопасный файл состояния"));
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn lock(path: &Path) -> io::Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}
fn locked(path: &Path) -> io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let file = lock(path)?;
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}
#[derive(Default)]
struct Active {
    paths: Vec<PathBuf>,
    checks: Vec<PathBuf>,
    running: bool,
}
impl Active {
    fn collect(state: &Path, home: &Path) -> io::Result<Self> {
        let mut active = Self::default();
        for job in children(&state.join("jobs"))? {
            if !fs::symlink_metadata(&job)?.is_dir() {
                continue;
            }
            let name = job.file_name().unwrap().to_string_lossy();
            let running = crate::jobs::result_in(state, &name, None, 4)
                .map_or(true, |r| matches!(r.state.as_str(), "running" | "starting"));
            if running {
                active.running = true;
                if let Ok(cwd) = fs::read_to_string(job.join("cwd")) {
                    active.paths.push(PathBuf::from(cwd));
                }
            }
        }
        for check in children(&state.join("checks"))? {
            if !fs::symlink_metadata(&check)?.is_dir() {
                continue;
            }
            let has_worker = locked(&check.join("worker.lock"))?;
            let mut unfinished = false;
            for target in children(&check)? {
                if target
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("target-"))
                {
                    if let Ok(value) = json_file(&target) {
                        if !matches!(
                            value["state"].as_str(),
                            Some("succeeded" | "failed" | "cancelled" | "unavailable")
                        ) || value["cleanup_pending"] == true
                        {
                            unfinished = true;
                            if let Some(workspace) = value["source"]["workspace"].as_str() {
                                active.paths.push(home.join("ferry").join(workspace));
                            }
                        }
                    } else {
                        unfinished = true;
                    }
                }
            }
            if has_worker || unfinished {
                active.running = true;
                active.checks.push(check.clone());
                if let Ok(op) = json_file(&check.join("operation.json"))
                    && let Some(cwd) = op["cwd"].as_str()
                {
                    active.paths.push(cwd.into());
                }
            }
        }
        for workspace in children(&home.join("ferry"))? {
            if let Some(name) = workspace.file_name()
                && locked(&state.join("workspace-locks").join(name))?
            {
                active.paths.push(workspace);
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            active.paths.push(exe);
        }
        Ok(active)
    }
    fn reason(&self, path: &Path, kind: Kind) -> Option<String> {
        if self
            .paths
            .iter()
            .any(|p| path.starts_with(p) || p.starts_with(path))
        {
            return Some("рабочий каталог или исполняемый файл активной задачи".into());
        }
        if self
            .checks
            .iter()
            .any(|p| path.starts_with(p) || p.starts_with(path))
        {
            return Some("незавершённая проверка: сначала check resume/cancel".into());
        }
        if kind == Kind::Runners && self.running {
            return Some("есть активные задачи или проверки".into());
        }
        None
    }
}
fn unit(scan: &mut Scan, path: &Path, kind: Kind, active: &Active) -> io::Result<Unit> {
    let mut m = Measure::default();
    scan.measure(path, &mut m, 0)?;
    let mut protected = active.reason(path, kind);
    if fs::symlink_metadata(path)?.is_symlink() {
        protected = Some("ссылка: внешние каталоги не обходятся".into());
    }
    if !kind.removable() {
        protected = Some(
            if kind == Kind::Archives {
                "архивы: используй history с защитой pinned"
            } else {
                "исходники и текущие задачи не удаляются"
            }
            .into(),
        );
    }
    if matches!(kind, Kind::Caches | Kind::Outputs)
        && let Some(reason) = source_protection(path, kind)
    {
        protected = Some(reason);
    }
    let recent = if kind == Kind::Observations { 300 } else { 60 };
    if crate::now().saturating_sub(m.modified) < recent {
        protected = Some(format!("изменялось менее {recent} секунд назад"));
    }
    if kind == Kind::Questions {
        match json_file(path) {
            Ok(v)
                if v["state"] == "pending"
                    && v["expires_at"].as_u64().unwrap_or(u64::MAX) > crate::now() =>
            {
                protected = Some("ожидается ответ человека".into())
            }
            Err(_) => protected = Some("не удалось прочитать состояние вопроса".into()),
            _ => {}
        }
    }
    if kind == Kind::Checks
        && path.join("operation.json").exists()
        && children(path)?.iter().all(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("target-"))
        })
    {
        protected = Some("проверка ещё не получила результаты".into());
    }
    Ok(Unit {
        id: identity(path, kind),
        kind,
        path: path.into(),
        bytes: m.bytes,
        files: m.files,
        modified: m.modified,
        fingerprint: format!("{:016x}", m.hash.finish()),
        protected,
    })
}
fn source_protection(path: &Path, kind: Kind) -> Option<String> {
    use std::process::{Command, Stdio};
    let parent = path.parent()?;
    let name = path.file_name()?;
    let status = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(parent)
            .args(args)
            .arg("--")
            .arg(name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    match status(&["ls-files", "--error-unmatch"]) {
        Ok(s) if s.success() => return Some("каталог содержит отслеживаемые git файлы".into()),
        Ok(s) if s.code() == Some(1) => {}
        _ => return Some("не удалось исключить исходники через git".into()),
    }
    if kind == Kind::Caches && !status(&["check-ignore", "--quiet"]).is_ok_and(|s| s.success()) {
        return Some("кеш не исключён из git; автоматическое удаление запрещено".into());
    }
    None
}
fn identity(path: &Path, kind: Kind) -> String {
    let mut hash = DefaultHasher::new();
    path.hash(&mut hash);
    kind.hash(&mut hash);
    format!("s-{:016x}", hash.finish())
}
fn project(scan: &mut Scan, root: &Path, active: &Active, units: &mut Vec<Unit>) -> io::Result<()> {
    if fs::symlink_metadata(root)?.is_symlink() {
        units.push(unit(scan, root, Kind::Projects, active)?);
        return Ok(());
    }
    let kind = if root.join(".git/ferry-workspace.json").exists() {
        Kind::Workspaces
    } else {
        Kind::Projects
    };
    let mut retained = Measure::default();
    project_walk(scan, root, active, units, &mut retained, 0)?;
    units.push(Unit {
        id: identity(root, kind),
        kind,
        path: root.into(),
        bytes: retained.bytes,
        files: retained.files,
        modified: retained.modified,
        fingerprint: format!("{:016x}", retained.hash.finish()),
        protected: Some("исходники не удаляются; caches/outputs учитываются отдельно".into()),
    });
    Ok(())
}
fn project_walk(
    scan: &mut Scan,
    path: &Path,
    active: &Active,
    units: &mut Vec<Unit>,
    retained: &mut Measure,
    depth: usize,
) -> io::Result<()> {
    scan.tick(depth)?;
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        let parent = path.parent().unwrap_or(path);
        let name = path.file_name().unwrap_or_default();
        let cache = (name == "target" && parent.join("Cargo.toml").is_file())
            || (name == "node_modules" && parent.join("package.json").is_file());
        if cache {
            units.push(unit(scan, path, Kind::Caches, active)?);
            return Ok(());
        }
        if name == ".ferry-checks" {
            for child in children(path)? {
                units.push(unit(scan, &child, Kind::Outputs, active)?);
            }
            return Ok(());
        }
        for child in children(path)? {
            project_walk(scan, &child, active, units, retained, depth + 1)?;
        }
    } else {
        retained.bytes = retained.bytes.saturating_add(meta.len());
        retained.files += 1;
    }
    retained.modified = retained.modified.max(seconds(meta.modified()?));
    path.hash(&mut retained.hash);
    meta.len().hash(&mut retained.hash);
    meta.modified()?.hash(&mut retained.hash);
    Ok(())
}
fn inventory(state: &Path, home: &Path) -> io::Result<Vec<Unit>> {
    let active = Active::collect(state, home)?;
    let mut scan = Scan {
        entries: 0,
        started: Instant::now(),
    };
    let mut units = Vec::new();
    for (folder, kind) in [
        ("shots", Kind::Shots),
        ("observations", Kind::Observations),
        ("recs", Kind::Recordings),
        ("checks", Kind::Checks),
        ("diagnostics", Kind::Diagnostics),
        ("runners", Kind::Runners),
        ("questions", Kind::Questions),
    ] {
        let base = state.join(folder);
        if base.exists() && fs::symlink_metadata(&base)?.is_symlink() {
            units.push(unit(&mut scan, &base, kind, &active)?);
            continue;
        }
        for child in children(&base)? {
            units.push(unit(&mut scan, &child, kind, &active)?);
        }
    }
    for (folder, kind) in [("jobs", Kind::Jobs), ("runs", Kind::Archives)] {
        let base = state.join(folder);
        if base.exists() {
            units.push(unit(&mut scan, &base, kind, &active)?);
        }
    }
    let projects = home.join("ferry");
    if projects.exists() && fs::symlink_metadata(&projects)?.is_symlink() {
        units.push(unit(&mut scan, &projects, Kind::Projects, &active)?);
    } else {
        for root in children(&projects)? {
            project(&mut scan, &root, &active, &mut units)?;
        }
    }
    units.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(units)
}

pub fn execute(state: &Path, home: &Path, request: Request) -> io::Result<Value> {
    if request.apply.is_some()
        && (request.kind.is_none()
            || request.older_than_days.is_none() && request.max_bytes.is_none())
    {
        return Err(io::Error::other(
            "storage apply требует kind и политику очистки",
        ));
    }
    if request.kind.is_some_and(|k| !k.removable())
        && (request.older_than_days.is_some()
            || request.max_bytes.is_some()
            || request.apply.is_some())
    {
        return Err(io::Error::other(
            "эта категория доступна только для инвентаризации",
        ));
    }
    let guard = lock(&state.join("storage.lock"))?;
    guard.lock()?;
    // AtomicStart uses the same lock; a new worker cannot enter a selected cache mid-delete.
    let admission = lock(&state.join("job-locks/execution-admission"))?;
    if request.apply.is_some() {
        admission.lock()?;
    }
    let questions = lock(&state.join("questions.lock"))?;
    if request.apply.is_some() && request.kind == Some(Kind::Questions) {
        questions.lock()?;
    }
    let units = inventory(state, home)?;
    let mut totals: BTreeMap<Kind, u64> = BTreeMap::new();
    for u in &units {
        *totals.entry(u.kind).or_default() += u.bytes;
    }
    let selected: Vec<_> = units
        .iter()
        .filter(|u| {
            Some(u.kind) == request.kind && request.only.as_ref().is_none_or(|id| id == &u.id)
        })
        .collect();
    if request.only.is_some() && selected.len() != 1 {
        return Err(io::Error::other(
            "--only должен совпадать с одним id выбранной категории из storage",
        ));
    }
    let mut remaining = selected.iter().map(|u| u.bytes).sum::<u64>();
    let mut order = selected.clone();
    order.sort_by_key(|u| (u.modified, u.path.clone()));
    let mut candidates = Vec::new();
    let mut more_candidates = false;
    for u in order {
        let old = request
            .older_than_days
            .is_some_and(|days| crate::now().saturating_sub(u.modified) >= u64::from(days) * 86400);
        if u.protected.is_none()
            && u.kind.removable()
            && (old || request.max_bytes.is_some_and(|max| remaining > max))
        {
            if candidates.len() == 2000 {
                more_candidates = true;
                continue;
            }
            remaining = remaining.saturating_sub(u.bytes);
            candidates.push(u);
        }
    }
    let mut hash = DefaultHasher::new();
    request.kind.hash(&mut hash);
    request.only.hash(&mut hash);
    request.older_than_days.hash(&mut hash);
    request.max_bytes.hash(&mut hash);
    for u in &selected {
        u.path.hash(&mut hash);
        u.fingerprint.hash(&mut hash);
        u.protected.hash(&mut hash);
    }
    for u in &candidates {
        u.id.hash(&mut hash);
    }
    let token = format!("{:016x}", hash.finish());
    if request
        .apply
        .as_ref()
        .is_some_and(|expected| expected != &token)
    {
        return Err(io::Error::other(
            "storage preview устарел; файлы или активные задачи изменились",
        ));
    }
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    if request.apply.is_some() {
        for u in &candidates {
            let current = Active::collect(state, home)?;
            if let Some(reason) = current.reason(&u.path, u.kind) {
                errors.push(json!({"path":u.path,"error":reason}));
                continue;
            }
            let removed = (|| -> io::Result<()> {
                let meta = fs::symlink_metadata(&u.path)?;
                if meta.is_symlink() {
                    return Err(io::Error::other("каталог заменён ссылкой"));
                }
                let mut scan = Scan {
                    entries: 0,
                    started: Instant::now(),
                };
                let after = unit(&mut scan, &u.path, u.kind, &current)?;
                if after.fingerprint != u.fingerprint || after.protected.is_some() {
                    return Err(io::Error::other("кандидат изменился после preview"));
                }
                if meta.is_dir() {
                    fs::remove_dir_all(&u.path)
                } else {
                    fs::remove_file(&u.path)
                }
            })();
            match removed {
                Ok(()) => deleted.push(json!({"id":u.id,"path":u.path,"bytes":u.bytes})),
                Err(e) => errors.push(json!({"path":u.path,"error":e.to_string()})),
            }
        }
    }
    let bytes = units.iter().map(|u| u.bytes).sum::<u64>();
    let freed = deleted
        .iter()
        .map(|v| v["bytes"].as_u64().unwrap_or(0))
        .sum::<u64>();
    let entries: Vec<_> = units
        .iter()
        .filter(|u| {
            request.kind.is_none_or(|k| k == u.kind)
                && request.only.as_ref().is_none_or(|id| id == &u.id)
        })
        .collect();
    let count = entries.len();
    let entries: Vec<_> = entries.into_iter().take(2000).collect();
    let actual_remaining = selected
        .iter()
        .map(|u| u.bytes)
        .sum::<u64>()
        .saturating_sub(freed);
    Ok(
        json!({"measured_at":crate::now(),"scan_complete":true,"logical_bytes":bytes,"categories":totals,"entries":entries,"entry_count":count,"entries_truncated":count>2000,"kind":request.kind,"only":request.only,"candidates":candidates,"more_candidates":more_candidates,"preview_token":token,"planned_remaining_bytes":remaining,"actual_remaining_bytes":actual_remaining,"budget_met":request.max_bytes.is_none_or(|max|if request.apply.is_some(){actual_remaining<=max}else{remaining<=max}),"applied":request.apply.is_some(),"deleted":deleted,"freed_bytes":freed,"errors":errors,"note":"Логические размеры файлов; исходники, архивы pinned и idempotency receipts не удаляются. Файлы моложе минуты и observations моложе 5 минут защищены. Активные задачи ferry защищены; процессы сборки, запущенные вне ferry, не обнаруживаются — перед очисткой их нужно завершить."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("ferry-storage-{}", crate::jobs::unique_id()));
        let state = root.join("state");
        let home = root.join("home");
        let project = home.join("ferry/app");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(project.join("target")).unwrap();
        fs::write(project.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(project.join("source.rs"), "valuable source").unwrap();
        fs::write(project.join(".gitignore"), "/target/\n/node_modules/\n").unwrap();
        fs::write(project.join("target/object"), "generated object").unwrap();
        git(&project, &["init", "-q"]);
        git(&project, &["add", "Cargo.toml", "source.rs", ".gitignore"]);
        old(&project.join("target"));
        (root, state, home)
    }
    fn git(dir: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    fn old(path: &Path) {
        if fs::symlink_metadata(path).unwrap().is_dir() {
            for child in children(path).unwrap() {
                old(&child);
            }
        }
        #[cfg(unix)]
        let file = fs::File::open(path).unwrap();
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .access_mode(0x0100)
                .custom_flags(0x02000000)
                .open(path)
                .unwrap()
        };
        file.set_times(
            fs::FileTimes::new().set_modified(UNIX_EPOCH + std::time::Duration::from_secs(1)),
        )
        .unwrap();
    }
    fn request(kind: Kind) -> Request {
        Request {
            kind: Some(kind),
            only: None,
            older_than_days: Some(0),
            max_bytes: None,
            apply: None,
        }
    }
    #[test]
    fn exact_cache_preview_detects_changes_and_keeps_source() {
        let (root, state, home) = fixture();
        let project = home.join("ferry/app");
        let preview = execute(&state, &home, request(Kind::Caches)).unwrap();
        assert_eq!(preview["candidates"].as_array().unwrap().len(), 1);
        let mut q = request(Kind::Caches);
        q.only = Some(preview["candidates"][0]["id"].as_str().unwrap().into());
        let preview = execute(&state, &home, q.clone()).unwrap();
        q.apply = Some(preview["preview_token"].as_str().unwrap().into());
        fs::write(project.join("target/object"), "changed output").unwrap();
        assert!(execute(&state, &home, q.clone()).is_err());
        assert!(project.join("target/object").exists());
        old(&project.join("target"));
        q.apply = None;
        let preview = execute(&state, &home, q.clone()).unwrap();
        q.apply = Some(preview["preview_token"].as_str().unwrap().into());
        let applied = execute(&state, &home, q).unwrap();
        assert_eq!(applied["deleted"].as_array().unwrap().len(), 1);
        assert!(!project.join("target").exists());
        assert_eq!(
            fs::read_to_string(project.join("source.rs")).unwrap(),
            "valuable source"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn tracked_files_and_unfinished_checks_protect_caches() {
        let (root, state, home) = fixture();
        let project = home.join("ferry/app");
        git(&project, &["add", "-f", "target/object"]);
        assert!(
            execute(&state, &home, request(Kind::Caches)).unwrap()["candidates"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        git(&project, &["rm", "--cached", "-q", "target/object"]);
        let check = state.join("checks/unfinished");
        fs::create_dir_all(&check).unwrap();
        crate::write_private(
            &check.join("operation.json"),
            &json!({"cwd":project}).to_string(),
        )
        .unwrap();
        crate::write_private(
            &check.join("target-0.json"),
            &json!({"state":"incomplete","cleanup_pending":true}).to_string(),
        )
        .unwrap();
        old(&check);
        assert!(
            execute(&state, &home, request(Kind::Caches)).unwrap()["candidates"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            execute(&state, &home, request(Kind::Checks)).unwrap()["candidates"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn directory_links_never_enter_external_data() {
        let (root, state, home) = fixture();
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), "private").unwrap();
        std::os::unix::fs::symlink(&outside, home.join("ferry/app/node_modules")).unwrap();
        fs::write(home.join("ferry/app/package.json"), "{}").unwrap();
        let v = execute(&state, &home, request(Kind::Caches)).unwrap();
        assert_eq!(v["candidates"].as_array().unwrap().len(), 1);
        assert!(
            v["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["path"].as_str().unwrap() != outside.to_string_lossy())
        );
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "private");
        fs::remove_dir_all(root).unwrap();
    }
}
