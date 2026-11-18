//! Logical disk use and explicit cleanup of generated data. Source and run receipts stay.
use crate::jobs::execution::Source;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
    measurement_complete: bool,
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
    deadline: Instant,
    limit: usize,
}
impl Scan {
    fn unit(deadline: Instant) -> Self {
        Self {
            entries: 0,
            deadline: deadline.min(Instant::now() + Duration::from_secs(2)),
            limit: 100_000,
        }
    }
    fn tick(&mut self, depth: usize) -> io::Result<()> {
        self.entries += 1;
        if self.entries > self.limit || depth > 64 || Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "storage: достигнут предел измерения",
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
        if metadata.is_dir() && !is_link(&metadata) {
            let mut children = Vec::new();
            for child in fs::read_dir(path)? {
                self.tick(depth + 1)?;
                children.push(child?.path());
            }
            children.sort();
            for child in children {
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
    if fs::symlink_metadata(path).is_ok_and(|m| is_link(&m)) {
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
    if is_link(&meta) || !meta.is_file() || meta.len() > 8 * 1024 * 1024 {
        return Err(io::Error::other("небезопасный файл состояния"));
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.is_symlink()
    }
}

fn no_link(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if is_link(&meta) => Err(io::Error::other(format!(
            "storage: ссылка или reparse point {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn safe_child(root: &Path, relative: &str) -> io::Result<PathBuf> {
    let relative = crate::project::relative_cache_path(relative)?;
    no_link(root)?;
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        no_link(&path)?;
    }
    Ok(path)
}

fn managed_root(home: &Path, source: &Source) -> io::Result<PathBuf> {
    crate::workspace::validate(source)?;
    for path in [
        home.to_path_buf(),
        home.join(".ferry"),
        home.join(".ferry/workspace-claims"),
        home.join("ferry"),
    ] {
        no_link(&path)?;
    }
    let claim = home
        .join(".ferry/workspace-claims")
        .join(format!("{}.json", source.workspace));
    let saved: Source = serde_json::from_value(json_file(&claim)?)?;
    if saved != *source {
        return Err(io::Error::other(
            "storage: workspace принадлежит другому снимку",
        ));
    }
    let name = crate::workspace::project_name(home, &source.workspace)?;
    let root = home.join("ferry").join(name.as_ref());
    no_link(&root)?;
    no_link(&root.join(".git"))?;
    no_link(&root.join(".git/ferry-workspace.json"))?;
    if !root.is_dir() {
        return Err(io::Error::other(
            "storage: управляемая workspace отсутствует",
        ));
    }
    Ok(root)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachePolicy {
    source: Source,
    paths: Vec<String>,
    extras: Vec<String>,
}

/// Политика лишь добавляет кандидатов inventory; удаление требует отдельного preview/apply.
pub fn save_workspace_caches(
    state: &Path,
    home: &Path,
    source: &Source,
    paths: &[String],
    extras: &[String],
) -> io::Result<()> {
    crate::project::validate_caches(paths, extras)?;
    let guard = lock(&state.join("storage.lock"))?;
    guard.lock()?;
    let root = managed_root(home, source)?;
    for relative in paths {
        let path = safe_child(&root, relative)?;
        if fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_dir()) {
            return Err(io::Error::other("caches: нужен каталог, не файл"));
        }
    }
    no_link(state)?;
    let folder = state.join("storage-policies");
    no_link(&folder)?;
    fs::create_dir_all(&folder)?;
    let path = folder.join(format!("{}.json", source.workspace));
    no_link(&path)?;
    let policy = CachePolicy {
        source: source.clone(),
        paths: paths.to_vec(),
        extras: extras.to_vec(),
    };
    crate::write_private(&path, &serde_json::to_string(&policy)?)
}

fn workspace_caches(
    state: &Path,
    home: &Path,
    root: &Path,
) -> io::Result<(Vec<PathBuf>, Vec<String>)> {
    let marker = root.join(".git/ferry-workspace.json");
    if !marker.exists() {
        return Ok((vec![], vec![]));
    }
    no_link(&root.join(".git"))?;
    let source: Source = serde_json::from_value(json_file(&marker)?)?;
    crate::workspace::validate(&source)?;
    let folder = state.join("storage-policies");
    no_link(&folder)?;
    let path = folder.join(format!("{}.json", source.workspace));
    if !path.exists() {
        return Ok((vec![], vec![]));
    }
    let policy: CachePolicy = serde_json::from_value(json_file(&path)?)?;
    if policy.source != source || managed_root(home, &source)? != root {
        return Err(io::Error::other(
            "storage: политика принадлежит другой workspace",
        ));
    }
    crate::workspace::verify(root, &source)?;
    crate::project::validate_caches(&policy.paths, &policy.extras)?;
    let paths = policy
        .paths
        .iter()
        .map(|path| safe_child(root, path))
        .collect::<io::Result<Vec<_>>>()?;
    Ok((paths, policy.extras))
}

struct ContentsScan {
    entries: usize,
    started: Instant,
}
impl ContentsScan {
    fn measure(&mut self, path: &Path, depth: usize) -> io::Result<Option<u64>> {
        self.entries += 1;
        if self.entries > 100_000 || depth > 32 || self.started.elapsed().as_secs() >= 5 {
            return Ok(None);
        }
        let meta = fs::symlink_metadata(path)?;
        if is_link(&meta) {
            return Ok(None);
        }
        if meta.is_file() {
            return Ok(Some(meta.len()));
        }
        if !meta.is_dir() {
            return Ok(None);
        }
        let mut bytes = 0u64;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_name().eq_ignore_ascii_case(".git") {
                continue;
            }
            let Some(size) = self.measure(&entry.path(), depth + 1)? else {
                return Ok(None);
            };
            bytes = bytes.saturating_add(size);
        }
        Ok(Some(bytes))
    }
}

/// Метаданные одного уровня. bytes=null означает, что полный размер не уложился в лимит.
pub fn project_contents(home: &Path, source: &Source, directory: &str) -> io::Result<Value> {
    let root = managed_root(home, source)?;
    crate::workspace::verify(&root, source)?;
    let path = if directory.is_empty() {
        root
    } else {
        safe_child(&root, directory)?
    };
    if !path.is_dir() {
        return Err(io::Error::other("contents: нужен каталог проекта"));
    }
    let mut scan = ContentsScan {
        entries: 0,
        started: Instant::now(),
    };
    let mut entries = Vec::new();
    let mut truncated = false;
    for (index, entry) in fs::read_dir(&path)?.enumerate() {
        if index >= 200 {
            truncated = true;
            break;
        }
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            truncated = true;
            continue;
        };
        if name.eq_ignore_ascii_case(".git") {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path())?;
        if is_link(&meta) || !(meta.is_file() || meta.is_dir()) {
            truncated = true;
            continue;
        }
        let relative = if directory.is_empty() {
            name.into()
        } else {
            format!("{directory}/{name}")
        };
        let bytes = if meta.is_file() {
            Some(meta.len())
        } else {
            scan.measure(&entry.path(), 0)?
        };
        truncated |= bytes.is_none();
        entries.push(json!({"name":name,"path":relative,"bytes":bytes,"directory":meta.is_dir()}));
    }
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!({"directory":directory,"entries":entries,"truncated":truncated}))
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

pub struct ActivityGuard {
    directory: PathBuf,
    lock: Option<fs::File>,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        // Windows не удаляет файл, пока открыт удерживающий его handle.
        self.lock.take();
        if no_link(&self.directory).is_err()
            || self
                .directory
                .parent()
                .is_some_and(|parent| no_link(parent).is_err())
        {
            return;
        }
        let _ = fs::remove_file(self.directory.join("activity.json"));
        let _ = fs::remove_file(self.directory.join("lock"));
        let _ = fs::remove_dir(&self.directory);
    }
}

/// Не даёт storage удалить данные foreground-команды или синхронизации.
pub fn protect_activity(state: &Path, cwd: &Path) -> io::Result<ActivityGuard> {
    let cwd = fs::canonicalize(cwd)?;
    let admission = lock(&state.join("job-locks/execution-admission"))?;
    admission.lock()?;
    no_link(state)?;
    let folder = state.join("storage-activity");
    no_link(&folder)?;
    fs::create_dir_all(&folder)?;
    let directory = folder.join(crate::jobs::unique_id());
    fs::create_dir(&directory)?;
    let mut guard = ActivityGuard {
        directory,
        lock: None,
    };
    let marker = lock(&guard.directory.join("lock"))?;
    marker.lock()?;
    guard.lock = Some(marker);
    crate::write_private(
        &guard.directory.join("activity.json"),
        &json!({"cwd":cwd}).to_string(),
    )?;
    Ok(guard)
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
        no_link(&state.join("storage-activity"))?;
        for marker in children(&state.join("storage-activity"))? {
            no_link(&marker)?;
            no_link(&marker.join("lock"))?;
            if !locked(&marker.join("lock"))? {
                continue;
            }
            let value = json_file(&marker.join("activity.json"))?;
            let cwd = value["cwd"]
                .as_str()
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .ok_or_else(|| io::Error::other("storage: неверная метка активной операции"))?;
            active.paths.push(cwd);
            active.running = true;
        }
        let copies = fs::canonicalize(home.join("ferry")).ok();
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
                    let cwd = PathBuf::from(cwd);
                    let cwd = fs::canonicalize(&cwd).unwrap_or(cwd);
                    // Задача из src/ может использовать соседний target всей копии.
                    let copy = copies.as_ref().and_then(|copies| {
                        cwd.strip_prefix(copies)
                            .ok()?
                            .components()
                            .next()
                            .map(|component| copies.join(component))
                    });
                    active.paths.push(copy.unwrap_or(cwd));
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
                                let name = crate::workspace::project_name(home, workspace)?;
                                active.paths.push(home.join("ferry").join(name.as_ref()));
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
        for folder in ["captures", "workflows", "outputs"] {
            for item in children(&state.join(folder))? {
                if !fs::symlink_metadata(&item)?.is_dir() {
                    continue;
                }
                let unfinished_capture = folder == "captures"
                    && json_file(&item.join("report.json")).map_or(true, |r| {
                        !matches!(r["state"].as_str(), Some("completed" | "failed"))
                    });
                if locked(&item.join("worker.lock"))? || unfinished_capture {
                    active.running = true;
                    active.checks.push(item);
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
        active.paths = active
            .paths
            .into_iter()
            .map(|path| fs::canonicalize(&path).unwrap_or(path))
            .collect();
        Ok(active)
    }
    fn reason(&self, path: &Path, kind: Kind) -> Option<String> {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.into());
        if self.paths.iter().any(|p| {
            path.starts_with(p)
                || p.starts_with(path)
                || canonical.starts_with(p)
                || p.starts_with(&canonical)
        }) {
            return Some("рабочий каталог или исполняемый файл активной задачи".into());
        }
        if self
            .checks
            .iter()
            .any(|p| path.starts_with(p) || p.starts_with(path))
        {
            return Some("активная операция или незавершённая проверка/запись".into());
        }
        if kind == Kind::Runners && self.running {
            return Some("есть активные задачи или проверки".into());
        }
        None
    }
}
fn unit(scan: &mut Scan, path: &Path, kind: Kind, active: &Active) -> io::Result<Unit> {
    let mut m = Measure::default();
    let measurement_complete = scan.measure(path, &mut m, 0).is_ok();
    let mut protected = active.reason(path, kind);
    if fs::symlink_metadata(path).is_ok_and(|meta| is_link(&meta)) {
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
    if measurement_complete
        && (kind == Kind::Caches
            || (kind == Kind::Outputs
                && path.components().any(|c| c.as_os_str() == ".ferry-checks")))
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
    if measurement_complete
        && kind == Kind::Checks
        && path.join("operation.json").exists()
        && children(path)?.iter().all(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("target-"))
        })
    {
        protected = Some("проверка ещё не получила результаты".into());
    }
    if !measurement_complete {
        protected = Some(
            "размер измерен не полностью (лимит или ошибка чтения); удаление запрещено".into(),
        );
    }
    Ok(Unit {
        id: identity(path, kind),
        kind,
        path: path.into(),
        bytes: m.bytes,
        files: m.files,
        modified: m.modified,
        fingerprint: if measurement_complete {
            format!("{:016x}", m.hash.finish())
        } else {
            "incomplete".into()
        },
        protected,
        measurement_complete,
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
struct ProjectCaches {
    root: PathBuf,
    paths: Vec<PathBuf>,
    extras: Vec<String>,
}

struct Inventory<'a> {
    request: &'a Request,
    active: Active,
    units: Vec<Unit>,
    deadline: Instant,
    truncated: bool,
}
impl Inventory<'_> {
    fn only_done(&self) -> bool {
        self.request.only.is_some() && !self.units.is_empty()
    }
    fn kind(&self, kind: Kind) -> bool {
        self.request.kind.is_none_or(|selected| selected == kind)
    }
    fn wants(&self, path: &Path, kind: Kind) -> bool {
        self.kind(kind)
            && self
                .request
                .only
                .as_ref()
                .is_none_or(|id| *id == identity(path, kind))
    }
    fn add(&mut self, path: &Path, kind: Kind, protection: Option<String>) -> io::Result<()> {
        if !self.wants(path, kind)
            || self
                .units
                .iter()
                .any(|unit| unit.kind == kind && unit.path == path)
        {
            return Ok(());
        }
        let mut entry = unit(&mut Scan::unit(self.deadline), path, kind, &self.active)?;
        self.truncated |= !entry.measurement_complete;
        if protection.is_some() && entry.measurement_complete {
            entry.protected = protection;
        }
        self.units.push(entry);
        Ok(())
    }
    fn project(&mut self, root: &Path, state: &Path, home: &Path) -> io::Result<()> {
        if Instant::now() >= self.deadline {
            self.truncated = true;
            return Ok(());
        }
        if is_link(&fs::symlink_metadata(root)?) {
            return self.add(root, Kind::Projects, None);
        }
        let kind = if root.join(".git/ferry-workspace.json").exists() {
            Kind::Workspaces
        } else {
            Kind::Projects
        };
        let retain = self.wants(root, kind);
        if !retain && !self.kind(Kind::Caches) && !self.kind(Kind::Outputs) {
            return Ok(());
        }
        let (paths, extras) = workspace_caches(state, home, root)?;
        let caches = ProjectCaches {
            root: root.into(),
            paths,
            extras,
        };
        for (name, manifest) in [("target", "Cargo.toml"), ("node_modules", "package.json")] {
            let cache = root.join(name);
            if cache.is_dir() && root.join(manifest).is_file() {
                self.cache(&cache, &caches)?;
                if self.only_done() {
                    return Ok(());
                }
            }
        }
        // Точные настроенные пути известны без обхода больших исходников CEF/vendor.
        for cache in &caches.paths {
            let inside_builtin = cache
                .ancestors()
                .skip(1)
                .take_while(|path| *path != root)
                .any(|path| {
                    let parent = path.parent().unwrap();
                    (path.file_name().is_some_and(|n| n == "target")
                        && parent.join("Cargo.toml").is_file())
                        || (path.file_name().is_some_and(|n| n == "node_modules")
                            && parent.join("package.json").is_file())
                });
            if cache.is_dir() && !inside_builtin {
                self.cache(cache, &caches)?;
                if self.only_done() {
                    return Ok(());
                }
            }
        }
        let mut retained = Measure::default();
        let mut discovery = Scan {
            entries: 0,
            deadline: self.deadline,
            limit: 1_000_000,
        };
        let complete = self
            .project_walk(root, &mut retained, &mut discovery, 0, &caches, retain)
            .is_ok();
        self.truncated |= !complete;
        if retain {
            self.units.push(Unit {
                id: identity(root, kind),
                kind,
                path: root.into(),
                bytes: retained.bytes,
                files: retained.files,
                modified: retained.modified,
                fingerprint: if complete {
                    format!("{:016x}", retained.hash.finish())
                } else {
                    "incomplete".into()
                },
                protected: Some(
                    if complete {
                        "исходники не удаляются; caches/outputs учитываются отдельно"
                    } else {
                        "размер исходников измерен не полностью; удаление запрещено"
                    }
                    .into(),
                ),
                measurement_complete: complete,
            });
        }
        Ok(())
    }
    fn cache(&mut self, path: &Path, caches: &ProjectCaches) -> io::Result<()> {
        let relative = path
            .strip_prefix(&caches.root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let protected = caches
            .extras
            .iter()
            .any(|extra| crate::project::paths_intersect(&relative, extra))
            .then(|| "каталог пересекается с защищённым files проекта".into());
        self.add(path, Kind::Caches, protected)
    }
    fn project_walk(
        &mut self,
        path: &Path,
        retained: &mut Measure,
        scan: &mut Scan,
        depth: usize,
        caches: &ProjectCaches,
        retain: bool,
    ) -> io::Result<()> {
        if self.only_done() {
            return Ok(());
        }
        scan.tick(depth)?;
        let meta = fs::symlink_metadata(path)?;
        if meta.is_dir() && !is_link(&meta) {
            let parent = path.parent().unwrap_or(path);
            let name = path.file_name().unwrap_or_default();
            let cache = (name == "target" && parent.join("Cargo.toml").is_file())
                || (name == "node_modules" && parent.join("package.json").is_file())
                || caches.paths.iter().any(|cache| cache == path);
            if cache {
                self.cache(path, caches)?;
                return Ok(());
            }
            if name == ".ferry-checks"
                || (name == "ferry-inputs"
                    && parent.file_name().is_some_and(|p| p == ".git")
                    && parent.join("ferry-workspace.json").is_file())
            {
                if self.kind(Kind::Outputs) {
                    for child in children(path)? {
                        self.add(&child, Kind::Outputs, None)?;
                    }
                }
                return Ok(());
            }
            if !retain && name == ".git" {
                let inputs = path.join("ferry-inputs");
                if self.kind(Kind::Outputs) && inputs.is_dir() {
                    self.project_walk(&inputs, retained, scan, depth + 1, caches, false)?;
                }
                return Ok(());
            }
            let mut children = Vec::new();
            for child in fs::read_dir(path)? {
                scan.tick(depth + 1)?;
                let child = child?;
                // Directory enumeration already has type data; source files need no stat for caches.
                if retain || child.file_type()?.is_dir() {
                    children.push(child.path());
                }
            }
            children.sort_by_key(|child| {
                (
                    child
                        .file_name()
                        .is_none_or(|n| n != "target" && n != "node_modules"),
                    child.clone(),
                )
            });
            for child in children {
                self.project_walk(&child, retained, scan, depth + 1, caches, retain)?;
            }
        } else if retain {
            retained.bytes = retained.bytes.saturating_add(meta.len());
            retained.files += 1;
        }
        if retain {
            retained.modified = retained.modified.max(seconds(meta.modified()?));
            path.hash(&mut retained.hash);
            meta.len().hash(&mut retained.hash);
            meta.modified()?.hash(&mut retained.hash);
        }
        Ok(())
    }
}
fn inventory(state: &Path, home: &Path, request: &Request) -> io::Result<(Vec<Unit>, bool)> {
    let mut inventory = Inventory {
        request,
        active: Active::collect(state, home)?,
        units: vec![],
        deadline: Instant::now() + Duration::from_secs(20),
        truncated: false,
    };
    for (folder, kind) in [
        ("shots", Kind::Shots),
        ("observations", Kind::Observations),
        ("recs", Kind::Recordings),
        ("captures", Kind::Recordings),
        ("workflows", Kind::Diagnostics),
        ("outputs", Kind::Outputs),
        ("checks", Kind::Checks),
        ("diagnostics", Kind::Diagnostics),
        ("runners", Kind::Runners),
        ("questions", Kind::Questions),
    ] {
        if !inventory.kind(kind) {
            continue;
        }
        let base = state.join(folder);
        if base.exists() && is_link(&fs::symlink_metadata(&base)?) {
            inventory.add(&base, kind, None)?;
            continue;
        }
        for child in children(&base)? {
            inventory.add(&child, kind, None)?;
            if inventory.only_done() {
                break;
            }
        }
    }
    for (folder, kind) in [("jobs", Kind::Jobs), ("runs", Kind::Archives)] {
        let base = state.join(folder);
        if base.exists() {
            inventory.add(&base, kind, None)?;
        }
    }
    if !inventory.only_done()
        && [
            Kind::Projects,
            Kind::Workspaces,
            Kind::Caches,
            Kind::Outputs,
        ]
        .into_iter()
        .any(|kind| inventory.kind(kind))
    {
        let projects = home.join("ferry");
        if projects.exists() && is_link(&fs::symlink_metadata(&projects)?) {
            inventory.add(&projects, Kind::Projects, None)?;
        } else {
            for root in children(&projects)? {
                inventory.project(&root, state, home)?;
                if inventory.only_done() {
                    break;
                }
            }
        }
    }
    inventory.units.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((inventory.units, inventory.truncated))
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
    let (units, scan_truncated) = inventory(state, home, &request)?;
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
                if is_link(&meta) {
                    return Err(io::Error::other("каталог заменён ссылкой"));
                }
                let mut scan = Scan::unit(Instant::now() + Duration::from_secs(2));
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
    let scan_truncated = scan_truncated || count > 2000;
    let entries: Vec<_> = entries.into_iter().take(2000).collect();
    let actual_remaining = selected
        .iter()
        .map(|u| u.bytes)
        .sum::<u64>()
        .saturating_sub(freed);
    Ok(
        json!({"measured_at":crate::now(),"scan_complete":!scan_truncated,"scan_truncated":scan_truncated,"logical_bytes":bytes,"categories":totals,"entries":entries,"entry_count":count,"entries_truncated":count>2000,"kind":request.kind,"only":request.only,"candidates":candidates,"more_candidates":more_candidates,"preview_token":token,"planned_remaining_bytes":remaining,"actual_remaining_bytes":actual_remaining,"budget_met":!scan_truncated && request.max_bytes.is_none_or(|max|if request.apply.is_some(){actual_remaining<=max}else{remaining<=max}),"applied":request.apply.is_some(),"deleted":deleted,"freed_bytes":freed,"errors":errors,"note":"Логические размеры файлов; исходники, архивы pinned и idempotency receipts не удаляются. Файлы моложе минуты и observations моложе 5 минут защищены. Активные задачи ferry защищены; процессы сборки, запущенные вне ferry, не обнаруживаются — перед очисткой их нужно завершить."}),
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
    fn managed_fixture() -> (PathBuf, PathBuf, PathBuf, Source, PathBuf) {
        let (temp, state, home) = fixture();
        let project = home.join("ferry/app");
        fs::write(
            project.join(".gitignore"),
            "/target/\n/node_modules/\n/build/\n",
        )
        .unwrap();
        let repo = crate::sync::repo_at(&project).unwrap().unwrap();
        let (commit, tree) = crate::sync::snapshot(&repo).unwrap();
        let source = crate::workspace::source("app", "storage-test", &commit, &tree).unwrap();
        let prepared = crate::workspace::prepare_local(&home, &project, &source, &[]).unwrap();
        let root = PathBuf::from(prepared["cwd"].as_str().unwrap());
        (temp, state, home, source, root)
    }

    #[test]
    fn partial_cache_does_not_block_small_cache_preview_and_only_skips_it() {
        let (temp, state, home) = fixture();
        let project = home.join("ferry/app");
        let large = project.join("target");
        let mut deep = large.clone();
        for _ in 0..66 {
            deep = deep.join("d");
        }
        fs::create_dir_all(deep).unwrap();
        old(&large);
        fs::write(project.join("package.json"), "{}").unwrap();
        let small = project.join("node_modules");
        fs::create_dir(&small).unwrap();
        fs::write(small.join("object"), "small").unwrap();
        old(&small);
        let preview = execute(&state, &home, request(Kind::Caches)).unwrap();
        assert_eq!(preview["scan_truncated"], true);
        let incomplete = preview["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["path"] == json!(large))
            .unwrap();
        assert_eq!(incomplete["measurement_complete"], false);
        assert!(incomplete["protected"].is_string());
        assert_eq!(preview["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(preview["candidates"][0]["path"], json!(small));
        let mut only = request(Kind::Caches);
        only.only = Some(identity(&small, Kind::Caches));
        let exact = execute(&state, &home, only).unwrap();
        assert_eq!(exact["scan_complete"], true);
        assert_eq!(exact["entry_count"], 1);
        assert_eq!(exact["logical_bytes"], 5);
        let unrelated = execute(&state, &home, request(Kind::Shots)).unwrap();
        assert_eq!(unrelated["scan_complete"], true);
        assert_eq!(unrelated["entry_count"], 0);
        // Неполный fingerprint большого соседа не меняет разрешение удалить точного кандидата.
        fs::write(large.join("object"), "changed large cache").unwrap();
        let mut apply = request(Kind::Caches);
        apply.apply = Some(preview["preview_token"].as_str().unwrap().into());
        let result = execute(&state, &home, apply).unwrap();
        assert_eq!(result["deleted"].as_array().unwrap().len(), 1);
        assert!(large.is_dir());
        assert!(!small.exists());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn exact_cache_request_never_measures_unrelated_source_tree() {
        let (temp, state, home) = fixture();
        let project = home.join("ferry/app");
        let mut deep = project.join("vendor");
        for _ in 0..66 {
            deep = deep.join("d");
        }
        fs::create_dir_all(deep).unwrap();
        let mut q = request(Kind::Caches);
        q.only = Some(identity(&project.join("target"), Kind::Caches));
        let preview = execute(&state, &home, q).unwrap();
        assert_eq!(preview["scan_complete"], true);
        assert_eq!(preview["entry_count"], 1);
        assert_eq!(preview["candidates"].as_array().unwrap().len(), 1);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn foreground_activity_blocks_cleanup_until_drop_and_stale_markers_do_not() {
        let (temp, state, home) = fixture();
        let project = home.join("ferry/app");
        let guard = protect_activity(&state, &project).unwrap();
        let preview = execute(&state, &home, request(Kind::Caches)).unwrap();
        assert!(preview["candidates"].as_array().unwrap().is_empty());
        assert!(
            preview["entries"][0]["protected"]
                .as_str()
                .unwrap()
                .contains("активной")
        );
        drop(guard);
        let stale = state.join("storage-activity/stale");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("activity.json"), "broken stale data").unwrap();
        let preview = execute(&state, &home, request(Kind::Caches)).unwrap();
        assert_eq!(preview["candidates"].as_array().unwrap().len(), 1);
        let mut q = request(Kind::Caches);
        q.apply = Some(preview["preview_token"].as_str().unwrap().into());
        let guard = protect_activity(&state, &project).unwrap();
        assert!(execute(&state, &home, q.clone()).is_err());
        assert!(project.join("target/object").exists());
        drop(guard);
        assert_eq!(
            execute(&state, &home, q).unwrap()["deleted"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn durable_job_in_subdirectory_protects_its_whole_ferry_copy_only() {
        let (temp, state, home) = fixture();
        let project = home.join("ferry/app");
        let cwd = project.join("src");
        fs::create_dir(&cwd).unwrap();
        let job = state.join("jobs/subdirectory");
        let guard = lock(&job.join("lock")).unwrap();
        guard.lock().unwrap();
        fs::write(job.join("run-id"), "subdirectory-id").unwrap();
        fs::write(job.join("log"), "running").unwrap();
        fs::write(job.join("cwd"), cwd.to_string_lossy().as_bytes()).unwrap();
        let preview = execute(&state, &home, request(Kind::Caches)).unwrap();
        assert!(preview["candidates"].as_array().unwrap().is_empty());
        assert!(
            preview["entries"][0]["protected"]
                .as_str()
                .unwrap()
                .contains("активной")
        );

        let external = home.join("work");
        fs::create_dir_all(external.join("src")).unwrap();
        fs::create_dir(external.join("target")).unwrap();
        fs::write(
            job.join("cwd"),
            external.join("src").to_string_lossy().as_bytes(),
        )
        .unwrap();
        let active = Active::collect(&state, &home).unwrap();
        assert!(active.reason(&external.join("src"), Kind::Caches).is_some());
        assert!(
            active
                .reason(&external.join("target"), Kind::Caches)
                .is_none()
        );
        assert_eq!(
            execute(&state, &home, request(Kind::Caches)).unwrap()["candidates"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        drop(guard);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn configured_caches_still_require_policy_preview_and_untracked_ignored_data() {
        let (temp, state, home, source, root) = managed_fixture();
        fs::create_dir(root.join("build")).unwrap();
        fs::write(root.join("build/object"), "generated").unwrap();
        old(&root.join("build"));
        let before = execute(&state, &home, request(Kind::Caches)).unwrap();
        assert!(
            !before["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["path"] == json!(root.join("build")))
        );
        save_workspace_caches(&state, &home, &source, &["build".into()], &[".env".into()]).unwrap();
        let preview = execute(&state, &home, request(Kind::Caches)).unwrap();
        let candidate = preview["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["path"] == json!(root.join("build")))
            .unwrap();
        let mut q = request(Kind::Caches);
        q.only = Some(candidate["id"].as_str().unwrap().into());
        let preview = execute(&state, &home, q.clone()).unwrap();
        q.apply = Some(preview["preview_token"].as_str().unwrap().into());
        save_workspace_caches(&state, &home, &source, &[], &[]).unwrap();
        assert!(execute(&state, &home, q.clone()).is_err());
        assert!(root.join("build/object").is_file());
        save_workspace_caches(&state, &home, &source, &["build".into()], &[]).unwrap();
        git(&root, &["add", "-f", "build/object"]);
        q.apply = None;
        let protected = execute(&state, &home, q.clone()).unwrap();
        assert!(protected["candidates"].as_array().unwrap().is_empty());
        assert!(
            protected["entries"][0]["protected"]
                .as_str()
                .unwrap()
                .contains("отслеживаемые")
        );
        git(&root, &["rm", "--cached", "-q", "build/object"]);
        old(&root.join("build"));
        let preview = execute(&state, &home, q.clone()).unwrap();
        q.apply = Some(preview["preview_token"].as_str().unwrap().into());
        assert_eq!(
            execute(&state, &home, q).unwrap()["deleted"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(!root.join("build").exists());
        assert!(root.join("source.rs").is_file());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn extras_and_active_workspace_protect_automatic_caches_too() {
        let (temp, state, home, source, root) = managed_fixture();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("target/private"), "keep").unwrap();
        old(&root.join("target"));
        save_workspace_caches(&state, &home, &source, &[], &["target/private".into()]).unwrap();
        let report = execute(&state, &home, request(Kind::Caches)).unwrap();
        let entry = report["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["path"] == json!(root.join("target")))
            .unwrap();
        assert!(entry["protected"].as_str().unwrap().contains("files"));
        save_workspace_caches(&state, &home, &source, &[], &[]).unwrap();
        let guard =
            crate::workspace::lock(&state, root.file_name().unwrap().to_str().unwrap()).unwrap();
        let report = execute(&state, &home, request(Kind::Caches)).unwrap();
        let entry = report["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["path"] == json!(root.join("target")))
            .unwrap();
        assert!(entry["protected"].as_str().unwrap().contains("активной"));
        drop(guard);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn contents_stay_in_exact_workspace_and_report_incomplete_sizes() {
        let (temp, state, home, source, root) = managed_fixture();
        fs::create_dir(root.join("build")).unwrap();
        fs::write(root.join("build/object"), "12345").unwrap();
        let listed = project_contents(&home, &source, "").unwrap();
        assert!(
            !listed["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == ".git")
        );
        let inside = project_contents(&home, &source, "build").unwrap();
        assert_eq!(
            inside["entries"][0],
            json!({"name":"object","path":"build/object","bytes":5,"directory":false})
        );
        for path in ["../app", ".git", "/tmp", "build/../.git"] {
            assert!(project_contents(&home, &source, path).is_err());
            assert!(save_workspace_caches(&state, &home, &source, &[path.into()], &[]).is_err());
        }
        assert!(
            save_workspace_caches(
                &state,
                &home,
                &source,
                &["build".into()],
                &["build/object".into()]
            )
            .is_err()
        );
        let mut wrong = source.clone();
        wrong.tree = "a".repeat(40);
        assert!(project_contents(&home, &wrong, "").is_err());
        assert!(save_workspace_caches(&state, &home, &wrong, &["build".into()], &[]).is_err());
        let mut deep = root.join("deep");
        for _ in 0..35 {
            deep = deep.join("d");
        }
        fs::create_dir_all(deep).unwrap();
        let listed = project_contents(&home, &source, "").unwrap();
        let deep = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "deep")
            .unwrap();
        assert!(deep["bytes"].is_null());
        assert_eq!(listed["truncated"], true);
        fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn configured_paths_and_contents_never_follow_links() {
        let (temp, state, home, source, root) = managed_fixture();
        let outside = temp.join("outside");
        fs::create_dir_all(outside.join("cache")).unwrap();
        fs::write(outside.join("cache/private"), "keep").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(
            save_workspace_caches(&state, &home, &source, &["link/cache".into()], &[]).is_err()
        );
        assert!(project_contents(&home, &source, "link").is_err());
        let listed = project_contents(&home, &source, "").unwrap();
        assert!(
            !listed["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == "link")
        );
        assert_eq!(listed["truncated"], true);
        assert_eq!(
            fs::read_to_string(outside.join("cache/private")).unwrap(),
            "keep"
        );
        fs::remove_dir_all(temp).unwrap();
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
    fn runner_cleanup_preserves_active_and_recent_copies() {
        let (root, state, home) = fixture();
        let stale = state.join("runners/previous");
        let fresh = state.join("runners/current");
        for runner in [&stale, &fresh] {
            fs::create_dir_all(runner).unwrap();
            fs::write(runner.join("ferry-job.exe"), b"runner").unwrap();
            fs::write(runner.join("WebView2Loader.dll"), b"dll").unwrap();
        }
        old(&stale);
        let mut q = request(Kind::Runners);
        q.older_than_days = Some(7);
        let preview = execute(&state, &home, q.clone()).unwrap();
        assert_eq!(preview["categories"]["runners"], 18);
        assert_eq!(preview["entry_count"], 2);
        assert_eq!(preview["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(
            Path::new(preview["candidates"][0]["path"].as_str().unwrap()),
            stale
        );

        let job = state.join("jobs/live");
        let guard = lock(&job.join("lock")).unwrap();
        guard.lock().unwrap();
        fs::write(job.join("run-id"), "live-id").unwrap();
        fs::write(job.join("log"), "running").unwrap();
        assert_eq!(
            crate::jobs::result_in(&state, "live", None, 4)
                .unwrap()
                .state,
            "running"
        );
        q.apply = Some(preview["preview_token"].as_str().unwrap().into());
        assert!(
            execute(&state, &home, q.clone())
                .unwrap_err()
                .to_string()
                .contains("preview устарел")
        );
        assert!(stale.join("ferry-job.exe").is_file());
        q.apply = None;
        let protected = execute(&state, &home, q.clone()).unwrap();
        assert!(protected["candidates"].as_array().unwrap().is_empty());

        drop(guard);
        fs::write(job.join("exit"), "0").unwrap();
        let preview = execute(&state, &home, q.clone()).unwrap();
        q.apply = Some(preview["preview_token"].as_str().unwrap().into());
        let applied = execute(&state, &home, q).unwrap();
        assert_eq!(applied["freed_bytes"], 9);
        assert!(!stale.exists());
        assert!(fresh.join("ferry-job.exe").is_file());
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
    #[test]
    fn new_controller_outputs_stay_protected_until_workers_finish() {
        let (root, state, home) = fixture();
        for (folder, kind) in [
            ("captures", Kind::Recordings),
            ("workflows", Kind::Diagnostics),
            ("outputs", Kind::Outputs),
        ] {
            let item = state.join(folder).join("own-fixture");
            fs::create_dir_all(&item).unwrap();
            fs::write(item.join("report.json"), "{\"state\":\"completed\"}").unwrap();
            let guard = lock(&item.join("worker.lock")).unwrap();
            guard.lock().unwrap();
            old(&item);
            assert!(
                execute(&state, &home, request(kind)).unwrap()["candidates"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            drop(guard);
            let preview = execute(&state, &home, request(kind)).unwrap();
            assert_eq!(preview["candidates"].as_array().unwrap().len(), 1);
            if folder == "captures" {
                fs::write(item.join("report.json"), "{\"state\":\"recording\"}").unwrap();
                old(&item);
                assert!(
                    execute(&state, &home, request(kind)).unwrap()["candidates"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
            }
        }
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
