//! Локальные квитанции передачи проекта и запуска на другом устройстве.

use std::collections::BTreeMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::jobs::execution::{Source, Started};

const HISTORY_LIMIT: usize = 100;
const RECORD_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    Outgoing,
    Incoming,
}

impl Direction {
    fn prefix(self) -> &'static str {
        match self {
            Self::Outgoing => "outgoing-",
            Self::Incoming => "incoming-",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Preparing,
    Transferring,
    Ready,
    Starting,
    Started,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transfer {
    pub id: String,
    pub direction: Direction,
    /// Полный ключ связанного устройства. Для входящей записи его берут из соединения.
    pub peer_id: String,
    pub project: String,
    pub state: State,
    pub created: u64,
    pub updated: u64,
    pub source: Option<Source>,
    pub cwd: Option<PathBuf>,
    pub run: Option<Started>,
    pub error: Option<String>,
}

impl Transfer {
    pub fn new(
        id: impl Into<String>,
        direction: Direction,
        peer_id: impl Into<String>,
        project: impl Into<String>,
    ) -> Self {
        let now = crate::now();
        Self {
            id: id.into(),
            direction,
            peer_id: peer_id.into(),
            project: project.into(),
            state: State::Preparing,
            created: now,
            updated: now,
            source: None,
            cwd: None,
            run: None,
            error: None,
        }
    }

    fn validate(&self) -> io::Result<()> {
        validate_id(&self.id)?;
        if self.peer_id.len() != 64
            || !self
                .peer_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(invalid("нужен полный ключ устройства"));
        }
        if self.project.trim().is_empty()
            || self.project.len() > 256
            || self.project.chars().any(char::is_control)
        {
            return Err(invalid("неверное имя проекта"));
        }
        if let Some(source) = &self.source {
            crate::workspace::validate(source)?;
        }
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub fn validate_id(id: &str) -> io::Result<()> {
    if id.len() > 48
        || !id.strip_prefix("gui-").is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
                })
        })
    {
        return Err(invalid("неверный ID передачи проекта"));
    }
    Ok(())
}

fn metadata(path: &Path) -> io::Result<Option<Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let linked = metadata.is_symlink();
    #[cfg(windows)]
    let linked = {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        linked || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    };
    if linked {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "журнал передачи не использует ссылки на файлы или каталоги",
        ));
    }
    Ok(Some(metadata))
}

fn directory(path: &Path, create: bool) -> io::Result<bool> {
    if create && metadata(path)?.is_none() {
        #[cfg(unix)]
        let created = {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(path)
        };
        #[cfg(not(unix))]
        let created = fs::create_dir(path);
        match created {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    match metadata(path)? {
        Some(metadata) if metadata.is_dir() => Ok(true),
        Some(_) => Err(invalid("журнал передачи должен быть каталогом")),
        None => Ok(false),
    }
}

fn folder(state: &Path, create: bool) -> io::Result<Option<PathBuf>> {
    if !directory(state, create)? {
        return Ok(None);
    }
    let path = state.join("project-transfers");
    Ok(directory(&path, create)?.then_some(path))
}

fn record_path(folder: &Path, id: &str, direction: Direction) -> PathBuf {
    folder.join(format!("{}{id}.json", direction.prefix()))
}

fn read(path: &Path, id: &str, direction: Direction) -> io::Result<Option<Transfer>> {
    let Some(metadata) = metadata(path)? else {
        return Ok(None);
    };
    if !metadata.is_file() || metadata.len() > RECORD_LIMIT as u64 {
        return Err(invalid("неверный файл квитанции передачи"));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(RECORD_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > RECORD_LIMIT {
        return Err(invalid("квитанция передачи слишком большая"));
    }
    let receipt: Transfer = serde_json::from_slice(&bytes)?;
    receipt.validate()?;
    if receipt.id != id || receipt.direction != direction {
        return Err(invalid("квитанция не совпадает с именем файла"));
    }
    Ok(Some(receipt))
}

pub fn save(state: &Path, receipt: &Transfer) -> io::Result<()> {
    receipt.validate()?;
    let text = serde_json::to_string(receipt)?;
    if text.len() > RECORD_LIMIT {
        return Err(invalid("квитанция передачи слишком большая"));
    }
    let folder = folder(state, true)?.ok_or_else(|| invalid("нет каталога квитанций"))?;
    let lock_path = folder.join(".write.lock");
    if metadata(&lock_path)?.is_some_and(|metadata| !metadata.is_file()) {
        return Err(invalid("неверный файл блокировки журнала передачи"));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock.lock()?;
    let path = record_path(&folder, &receipt.id, receipt.direction);
    if let Some(existing) = read(&path, &receipt.id, receipt.direction)?
        && (existing.peer_id != receipt.peer_id || existing.project != receipt.project)
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "ID передачи уже связан с другим устройством или проектом",
        ));
    }
    crate::write_private(&path, &text)
}

pub fn get(state: &Path, id: &str, direction: Direction) -> io::Result<Option<Transfer>> {
    validate_id(id)?;
    let Some(folder) = folder(state, false)? else {
        return Ok(None);
    };
    read(&record_path(&folder, id, direction), id, direction)
}

pub fn list(state: &Path) -> io::Result<Vec<Transfer>> {
    let Some(folder) = folder(state, false)? else {
        return Ok(Vec::new());
    };
    let mut recent = BTreeMap::new();
    for entry in fs::read_dir(folder)?.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        let Some((direction, id)) = [Direction::Outgoing, Direction::Incoming]
            .into_iter()
            .find_map(|direction| {
                stem.strip_prefix(direction.prefix())
                    .map(|id| (direction, id))
            })
        else {
            continue;
        };
        if validate_id(id).is_err() {
            continue;
        }
        if let Ok(Some(receipt)) = read(&entry.path(), id, direction) {
            let key = (
                receipt.updated,
                receipt.created,
                receipt.id.clone(),
                direction,
            );
            recent.insert(key, receipt);
            if recent.len() > HISTORY_LIMIT {
                recent.pop_first();
            }
        }
    }
    Ok(recent.into_values().rev().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("ferry-transfer-{}", crate::jobs::unique_id())))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn receipt(id: &str, direction: Direction) -> Transfer {
        Transfer::new(id, direction, "1".repeat(64), "project")
    }

    #[test]
    fn updates_are_private_and_directions_have_distinct_receipts() {
        let dir = Scratch::new();
        assert!(list(&dir.0).unwrap().is_empty());
        let outgoing = receipt("gui-one", Direction::Outgoing);
        let mut incoming = receipt("gui-one", Direction::Incoming);
        save(&dir.0, &outgoing).unwrap();
        save(&dir.0, &incoming).unwrap();
        incoming.state = State::Started;
        let source =
            crate::workspace::source("project", "gui-one", &"a".repeat(40), &"b".repeat(40))
                .unwrap();
        let cwd = PathBuf::from("isolated/project--gui-one");
        incoming.source = Some(source);
        incoming.cwd = Some(cwd.clone());
        incoming.run = Some(Started {
            request_id: "gui-one".into(),
            name: "check".into(),
            run_id: "run-one".into(),
            cwd,
            reused: false,
            state: "running".into(),
        });
        save(&dir.0, &incoming).unwrap();
        assert_eq!(
            get(&dir.0, "gui-one", Direction::Incoming).unwrap(),
            Some(incoming)
        );
        assert_eq!(
            get(&dir.0, "gui-one", Direction::Outgoing).unwrap(),
            Some(outgoing)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let folder = dir.0.join("project-transfers");
            assert_eq!(
                fs::metadata(&folder).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(folder.join("incoming-gui-one.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn shows_only_the_latest_hundred_and_ignores_bad_rows() {
        let dir = Scratch::new();
        for updated in (0..105).rev() {
            let mut row = receipt(&format!("gui-{updated}"), Direction::Incoming);
            row.updated = updated;
            save(&dir.0, &row).unwrap();
        }
        fs::write(
            dir.0.join("project-transfers/incoming-gui-bad.json"),
            "broken",
        )
        .unwrap();
        let rows = list(&dir.0).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.updated).collect::<Vec<_>>(),
            (5..105).rev().collect::<Vec<_>>()
        );
        assert!(get(&dir.0, "gui-0", Direction::Incoming).unwrap().is_some());
    }

    #[test]
    fn rejects_paths_and_competing_peers_cannot_replace_a_receipt() {
        let dir = Scratch::new();
        for bad in [
            "../elsewhere",
            "gui-../elsewhere",
            r"gui-..\elsewhere",
            "gui-",
            "gui-One",
        ] {
            assert!(save(&dir.0, &receipt(bad, Direction::Incoming)).is_err());
            assert!(get(&dir.0, bad, Direction::Incoming).is_err());
        }
        let a = receipt("gui-one", Direction::Incoming);
        let mut b = a.clone();
        b.peer_id = "2".repeat(64);
        let results = std::thread::scope(|scope| {
            let left = scope.spawn(|| save(&dir.0, &a));
            let right = scope.spawn(|| save(&dir.0, &b));
            [left.join().unwrap(), right.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|value| value.is_ok()).count(), 1);
        assert_eq!(
            results.into_iter().find_map(Result::err).unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_linked_receipt_folders_and_files() {
        use std::os::unix::fs::symlink;

        let dir = Scratch::new();
        let outside = Scratch::new();
        fs::create_dir(&dir.0).unwrap();
        fs::create_dir(&outside.0).unwrap();
        let folder = dir.0.join("project-transfers");
        let row = receipt("gui-one", Direction::Incoming);
        symlink(&outside.0, &folder).unwrap();
        assert!(save(&dir.0, &row).is_err());
        assert!(list(&dir.0).is_err());
        fs::remove_file(&folder).unwrap();
        save(&dir.0, &row).unwrap();
        let path = folder.join("incoming-gui-one.json");
        fs::remove_file(&path).unwrap();
        let outside_file = outside.0.join("untouched");
        fs::write(&outside_file, "outside").unwrap();
        symlink(&outside_file, &path).unwrap();
        assert!(get(&dir.0, &row.id, row.direction).is_err());
        assert!(save(&dir.0, &row).is_err());
        assert!(list(&dir.0).unwrap().is_empty());
        assert_eq!(fs::read_to_string(outside_file).unwrap(), "outside");
    }
}
