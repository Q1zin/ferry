//! История запусков и явно прикреплённые файлы. Перезапуск больше не стирает доказательства.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;

const FILE_LIMIT: u64 = 64 * 1024 * 1024;
const RUN_LIMIT: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Query {
    pub name: String,
    pub run_id: Option<String>,
    pub action: Action,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Action {
    Runs,
    List,
    Add {
        path: String,
    },
    Tests {
        artifact: String,
    },
    Result {
        cursor: Option<String>,
        max_bytes: u32,
    },
    Storage {
        older_than_days: Option<u32>,
        max_bytes: Option<u64>,
        apply: Option<String>,
    },
    Pin {
        pinned: bool,
    },
    Stop,
    Versions {
        shell: crate::desk::Shell,
        tools: Vec<String>,
    },
    Metadata,
    Probe {
        http: Option<super::probe::HttpProbe>,
        window: Option<String>,
    },
    StopOwned {
        request_id: String,
        session: String,
    },
}

pub(super) fn guard(root: &Path, name: &str) -> io::Result<fs::File> {
    validate_name(name)?;
    let locks = root.join("job-locks");
    fs::create_dir_all(&locks)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(locks.join(name))?;
    file.lock()?;
    Ok(file)
}

pub(super) fn archive(root: &Path, name: &str) -> io::Result<()> {
    let dir = root.join("jobs").join(name);
    if !dir.exists() {
        return Ok(());
    }
    // Незавершённый старт без лога тоже сохраняем; идентификатор фиксируется до rename.
    let id = result::generation(&dir).unwrap_or_else(|_| result::new_run_id());
    validate_name(&id)?;
    crate::write_private(&dir.join("run-id"), &id)?;
    let parent = root.join("runs").join(name);
    fs::create_dir_all(&parent)?;
    let dest = parent.join(id);
    if dest.exists() {
        return Err(io::Error::other(
            "архив запуска уже существует; старые данные сохранены",
        ));
    }
    fs::rename(dir, dest)
}

pub(super) fn locate(root: &Path, name: &str, id: Option<&str>) -> io::Result<PathBuf> {
    let current = root.join("jobs").join(name);
    match id {
        None => {
            if current.is_dir() {
                Ok(current)
            } else {
                Err(io::Error::other("нет задачи"))
            }
        }
        Some(id) => {
            validate_name(id)?;
            if result::generation(&current).ok().as_deref() == Some(id) {
                return Ok(current);
            }
            let archived = root.join("runs").join(name).join(id);
            if archived.is_dir() {
                Ok(archived)
            } else {
                Err(io::Error::other("нет такого запуска"))
            }
        }
    }
}

#[derive(Serialize)]
struct Artifact {
    id: String,
    name: String,
    path: PathBuf,
    bytes: u64,
    kind: &'static str,
}
fn artifacts(dir: &Path) -> io::Result<Vec<Artifact>> {
    let mut out = Vec::new();
    if let Ok(meta) = fs::metadata(dir.join("log")) {
        out.push(Artifact {
            id: "log".into(),
            name: "log".into(),
            path: dir.join("log"),
            bytes: meta.len(),
            kind: "log",
        });
    }
    let folder = dir.join("artifacts");
    if !folder.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(entry.path())? {
            let file = file?;
            if !file.file_type()?.is_file() {
                continue;
            }
            out.push(Artifact {
                id: entry.file_name().to_string_lossy().into(),
                name: file.file_name().to_string_lossy().into(),
                path: file.path(),
                bytes: file.metadata()?.len(),
                kind: "file",
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

pub fn execute(root: &Path, query: Query) -> io::Result<Value> {
    let _guard = guard(root, &query.name)?;
    if let Action::Storage {
        older_than_days,
        max_bytes,
        apply,
    } = &query.action
    {
        return super::history::storage(
            root,
            &query.name,
            *older_than_days,
            *max_bytes,
            apply.as_deref(),
        );
    }
    if query.action == Action::Runs {
        return runs(root, &query.name);
    }
    let dir = locate(root, &query.name, query.run_id.as_deref())?;
    let run_id = result::generation(&dir)?;
    let value = match query.action {
        Action::Runs | Action::Storage { .. } => unreachable!(),
        Action::StopOwned {
            request_id,
            session,
        } => {
            if query.run_id.is_none() {
                return Err(io::Error::other("остановка требует точный run_id"));
            }
            let saved = fs::read(dir.join("execution.json")).ok().and_then(|bytes| {
                serde_json::from_slice::<super::execution::StartRequest>(&bytes).ok()
            });
            let owned = saved
                .is_some_and(|r| r.request_id == request_id && r.session == session && !r.keep);
            if !owned || dir != root.join("jobs").join(&query.name) {
                json!({"stopped":false,"reason":"ownership-changed-or-archived"})
            } else {
                json!({"stopped":true,"message":super::stop_at(&query.name,&dir)?})
            }
        }
        Action::Metadata => json!({"metadata":super::execution::metadata(&dir)?}),
        Action::Probe { http, window } => {
            if query.run_id.is_none() {
                return Err(io::Error::other("probe требует точный run_id"));
            }
            super::probe::execute(&dir, http, window)?
        }
        Action::Versions { shell, tools } => crate::capabilities::versions(shell, tools)?,
        Action::Stop => {
            if query.run_id.is_none() || dir != root.join("jobs").join(&query.name) {
                return Err(io::Error::other("остановка требует id текущего запуска"));
            }
            json!({"message":super::stop_at(&query.name,&dir)?})
        }
        Action::Pin { pinned } => {
            if query.run_id.is_none() {
                return Err(io::Error::other("закрепление требует --run"));
            }
            let marker = dir.join("pinned");
            if pinned {
                crate::write_private(&marker, "pinned")?;
            } else if marker.exists() {
                fs::remove_file(marker)?;
            }
            json!({"pinned":pinned})
        }
        Action::Result { cursor, max_bytes } => serde_json::to_value(result::result_at(
            &dir,
            &query.name,
            cursor.as_deref(),
            max_bytes,
        )?)?,
        Action::List => {
            json!({"artifacts": artifacts(&dir)?, "result": result::result_at(&dir, &query.name, None, 1024)?})
        }
        Action::Add { path } => {
            let path = PathBuf::from(path);
            let source = if path.is_absolute() {
                path
            } else {
                PathBuf::from(
                    read(&dir, "cwd")
                        .ok_or_else(|| io::Error::other("нет рабочего каталога задачи"))?,
                )
                .join(path)
            };
            let metadata = fs::symlink_metadata(&source)?;
            if !metadata.is_file() {
                return Err(io::Error::other(
                    "прикрепить можно обычный файл, не каталог и не ссылку",
                ));
            }
            if metadata.len() > FILE_LIMIT {
                return Err(io::Error::other("артефакт больше 64 МиБ"));
            }
            let existing = artifacts(&dir)?;
            if existing.len() >= 257
                || existing
                    .iter()
                    .filter(|a| a.kind == "file")
                    .map(|a| a.bytes)
                    .sum::<u64>()
                    + metadata.len()
                    > RUN_LIMIT
            {
                return Err(io::Error::other(
                    "предел запуска: 256 файлов / 512 МиБ артефактов",
                ));
            }
            let name = source
                .file_name()
                .ok_or_else(|| io::Error::other("нужно имя файла"))?;
            let id = result::new_run_id();
            let staging = dir.join(format!(".artifact-{id}"));
            fs::create_dir(&staging)?;
            let copied = (|| -> io::Result<()> {
                let mut input = fs::File::open(&source)?.take(FILE_LIMIT + 1);
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(staging.join(name))?;
                let bytes = io::copy(&mut input, &mut output)?;
                if bytes > FILE_LIMIT || bytes != metadata.len() {
                    return Err(io::Error::other(
                        "файл изменился при копировании; повтори после завершения записи",
                    ));
                }
                let after = fs::metadata(&source)?;
                if after.len() != metadata.len()
                    || after.modified().ok() != metadata.modified().ok()
                {
                    return Err(io::Error::other(
                        "файл изменился при копировании; повтори после завершения записи",
                    ));
                }
                output.sync_all()?;
                drop(output);
                let folder = dir.join("artifacts");
                fs::create_dir_all(&folder)?;
                fs::rename(&staging, folder.join(&id))?;
                Ok(())
            })();
            if let Err(e) = copied {
                fs::remove_dir_all(&staging).ok();
                return Err(e);
            }
            json!({"artifact": artifacts(&dir)?.into_iter().find(|a| a.id == id).ok_or_else(|| io::Error::other("артефакт не сохранился"))?})
        }
        Action::Tests { artifact } => {
            let file = artifacts(&dir)?
                .into_iter()
                .find(|a| a.id == artifact)
                .ok_or_else(|| {
                    io::Error::other("нет артефакта: сначала artifacts --add <junit.xml>")
                })?;
            let mut report = serde_json::to_value(crate::junit::read(&file.path)?)?;
            report["artifact"] = serde_json::to_value(file)?;
            report
        }
    };
    let mut value = value;
    value["name"] = query.name.into();
    value["run_id"] = run_id.into();
    Ok(value)
}

fn runs(root: &Path, name: &str) -> io::Result<Value> {
    let mut dirs = Vec::new();
    let current = root.join("jobs").join(name);
    if current.is_dir() {
        dirs.push(current);
    }
    let history = root.join("runs").join(name);
    if history.exists() {
        for entry in fs::read_dir(history)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    let mut rows = Vec::new();
    for dir in dirs {
        let mut row = match result::result_at(&dir, name, None, 4) {
            Ok(result) => serde_json::to_value(result)?,
            Err(e) => {
                json!({"name":name,"run_id":result::generation(&dir).ok(),"started":read(&dir,"started").and_then(|s|s.parse::<u64>().ok()),"state":"unreadable","error":e.to_string()})
            }
        };
        for key in ["log", "cursor", "more", "omitted_bytes"] {
            row.as_object_mut().unwrap().remove(key);
        }
        row["pinned"] = dir.join("pinned").exists().into();
        row["archived"] = (dir.parent() != Some(root.join("jobs").as_path())).into();
        rows.push(row);
    }
    rows.sort_by(|a, b| {
        b["started"]
            .as_u64()
            .cmp(&a["started"].as_u64())
            .then_with(|| b["run_id"].as_str().cmp(&a["run_id"].as_str()))
    });
    let total = rows.len();
    rows.truncate(100);
    Ok(json!({"name":name, "runs":rows, "total":total, "truncated":total>100}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_preserves_logs_artifacts_and_rejects_unsafe_paths() {
        let root = std::env::temp_dir().join(result::new_run_id());
        let dir = root.join("jobs/build");
        fs::create_dir_all(&dir).unwrap();
        for (file, value) in [
            ("run-id", "old"),
            ("log", "proof"),
            ("exit", "7"),
            ("cwd", root.to_str().unwrap()),
        ] {
            fs::write(dir.join(file), value).unwrap();
        }
        fs::write(
            root.join("report.xml"),
            "<testsuite><testcase name=\"one\"/></testsuite>",
        )
        .unwrap();
        let query = |action| Query {
            name: "build".into(),
            run_id: Some("old".into()),
            action,
        };
        let added = execute(
            &root,
            query(Action::Add {
                path: "report.xml".into(),
            }),
        )
        .unwrap();
        let id = added["artifact"]["id"].as_str().unwrap();
        archive(&root, "build").unwrap();
        assert!(!dir.exists());
        assert_eq!(
            execute(
                &root,
                query(Action::Result {
                    cursor: None,
                    max_bytes: 64
                })
            )
            .unwrap()["log"],
            "proof"
        );
        let tests = execute(
            &root,
            query(Action::Tests {
                artifact: id.into(),
            }),
        )
        .unwrap();
        assert_eq!(tests["passed"], 1);
        assert_eq!(
            execute(&root, query(Action::Runs)).unwrap()["runs"][0]["archived"],
            true
        );
        assert!(
            execute(
                &root,
                Query {
                    run_id: Some("../bad".into()),
                    ..query(Action::List)
                }
            )
            .is_err()
        );
        assert!(
            execute(
                &root,
                query(Action::Add {
                    path: root.to_string_lossy().into()
                })
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
