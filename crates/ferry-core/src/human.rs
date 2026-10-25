//! Requests to the person at a paired device. Replies can only originate in the local UI.
pub mod presence;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Presence,
    Ask {
        id: String,
        text: String,
        timeout: u32,
    },
    Poll {
        id: String,
    },
    Cancel {
        id: String,
    },
}

fn valid_id(id: &str) -> io::Result<()> {
    if id.is_empty()
        || id.len() > 80
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(io::Error::other("id запроса: до 80 букв, цифр, - и _"));
    }
    Ok(())
}
fn path(root: &Path, id: &str) -> io::Result<PathBuf> {
    valid_id(id)?;
    Ok(root.join("questions").join(format!("{id}.json")))
}
fn lock(root: &Path) -> io::Result<std::fs::File> {
    std::fs::create_dir_all(root.join("questions"))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("questions.lock"))?;
    file.lock()?;
    Ok(file)
}
fn read(file: &Path) -> io::Result<Value> {
    if std::fs::symlink_metadata(file)?.len() > 16384 {
        return Err(io::Error::other("запрос слишком большой"));
    }
    let mut value: Value = serde_json::from_slice(&std::fs::read(file)?)?;
    if value["state"] == "pending" && value["expires_at"].as_u64().unwrap_or(0) <= crate::now() {
        value["state"] = "expired".into();
    }
    Ok(value)
}
fn pending_unlocked(root: &Path) -> io::Result<Vec<Value>> {
    let dir = root.join("questions");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut values = Vec::new();
    for file in std::fs::read_dir(dir)?.take(4097) {
        let file = file?;
        if file.file_type()?.is_file() && file.path().extension().is_some_and(|e| e == "json") {
            let value = read(&file.path())?;
            if value["state"] == "pending" {
                values.push(value);
            }
        }
    }
    values.sort_by_key(|v| v["created_at"].as_u64());
    Ok(values)
}
pub fn pending(root: &Path) -> io::Result<Vec<Value>> {
    let _lock = lock(root)?;
    pending_unlocked(root).map(|values| values.into_iter().map(public).collect())
}
fn public(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("owner");
    }
    value
}
pub fn execute(root: &Path, owner: &str, from_label: &str, request: Request) -> io::Result<Value> {
    if request == Request::Presence {
        return Ok(presence::read(root));
    }
    if owner.is_empty() || owner.len() > 512 {
        return Err(io::Error::other("нужна сессия отправителя"));
    }
    if let Request::Cancel { id } = &request {
        return cancel(root, owner, id);
    }
    let _lock = lock(root)?;
    match request {
        Request::Presence | Request::Cancel { .. } => unreachable!(),
        Request::Ask { id, text, timeout } => {
            if text.trim().is_empty()
                || text.len() > 2048
                || text.chars().count() > 512
                || !(30..=3600).contains(&timeout)
            {
                return Err(io::Error::other(
                    "вопрос: 1..512 символов; timeout 30..3600 с",
                ));
            }
            let file = path(root, &id)?;
            if file.exists() {
                let previous = read(&file)?;
                if previous["owner"] != owner
                    || previous["text"] != text
                    || previous["timeout"] != timeout
                {
                    return Err(io::Error::other("id уже относится к другому запросу"));
                }
                return Ok(public(previous));
            }
            if pending_unlocked(root)?.len() >= 8 {
                return Err(io::Error::other("у человека уже 8 ожидающих вопросов"));
            }
            if std::fs::read_dir(root.join("questions"))?.count() >= 4096 {
                return Err(io::Error::other(
                    "история вопросов заполнена; очисти завершённые запросы",
                ));
            }
            let now = crate::now();
            let value = json!({"id":id,"text":text,"from":from_label,"owner":owner,"timeout":timeout,
                "created_at":now,"expires_at":now+u64::from(timeout),"state":"pending"});
            crate::write_private(&file, &value.to_string())?;
            Ok(public(value))
        }
        Request::Poll { id } => {
            let file = path(root, &id)?;
            let value = read(&file)?;
            if value["owner"] != owner {
                return Err(io::Error::other("запрос принадлежит другой сессии"));
            }
            Ok(public(value))
        }
    }
}

pub fn cancel(root: &Path, owner: &str, id: &str) -> io::Result<Value> {
    let _lock = lock(root)?;
    let file = path(root, id)?;
    let mut value = read(&file)?;
    if value["owner"] != owner {
        return Err(io::Error::other("запрос принадлежит другой сессии"));
    }
    if value["state"] == "pending" {
        value["state"] = "cancelled".into();
        value["answered_at"] = crate::now().into();
        crate::write_private(&file, &value.to_string())?;
    }
    Ok(public(value))
}

/// Not exposed by the remote protocol: only the device's own Tauri window calls this.
pub fn answer(root: &Path, id: &str, answer: &str) -> io::Result<Value> {
    if !["done", "declined"].contains(&answer) {
        return Err(io::Error::other("ответ: done или declined"));
    }
    let _lock = lock(root)?;
    let file = path(root, id)?;
    let mut value = read(&file)?;
    if value["state"] != "pending" {
        return Err(io::Error::other("вопрос уже закрыт или истёк"));
    }
    value["state"] = answer.into();
    value["answered_at"] = crate::now().into();
    crate::write_private(&file, &value.to_string())?;
    Ok(public(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn questions_are_idempotent_owned_and_answered_only_once() {
        let root = std::env::temp_dir().join(format!("ferry-human-{}", crate::jobs::unique_id()));
        let ask = Request::Ask {
            id: "q1".into(),
            text: "Подтверди вход самостоятельно".into(),
            timeout: 60,
        };
        let first = execute(&root, "owner", "Mac", ask.clone()).unwrap();
        assert_eq!(first["state"], "pending");
        assert!(first["owner"].is_null());
        assert_eq!(execute(&root, "owner", "Mac", ask.clone()).unwrap(), first);
        assert!(execute(&root, "other", "Mac", ask).is_err());
        assert_eq!(pending(&root).unwrap().len(), 1);
        answer(&root, "q1", "done").unwrap();
        assert!(answer(&root, "q1", "declined").is_err());
        assert_eq!(
            execute(&root, "owner", "Mac", Request::Poll { id: "q1".into() }).unwrap()["state"],
            "done"
        );
        assert!(path(&root, "../q").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancellation_and_expiry_never_become_approval() {
        let root =
            std::env::temp_dir().join(format!("ferry-human-cancel-{}", crate::jobs::unique_id()));
        execute(
            &root,
            "owner",
            "Mac",
            Request::Ask {
                id: "cancel".into(),
                text: "Откройте окно".into(),
                timeout: 60,
            },
        )
        .unwrap();
        assert!(
            execute(
                &root,
                "other",
                "Mac",
                Request::Cancel {
                    id: "cancel".into()
                }
            )
            .is_err()
        );
        assert_eq!(
            execute(
                &root,
                "owner",
                "Mac",
                Request::Cancel {
                    id: "cancel".into()
                }
            )
            .unwrap()["state"],
            "cancelled"
        );
        assert!(answer(&root, "cancel", "done").is_err());
        execute(
            &root,
            "owner",
            "Mac",
            Request::Ask {
                id: "expired".into(),
                text: "Откройте окно".into(),
                timeout: 60,
            },
        )
        .unwrap();
        let file = path(&root, "expired").unwrap();
        let mut stored = read(&file).unwrap();
        stored["expires_at"] = 0.into();
        crate::write_private(&file, &stored.to_string()).unwrap();
        assert!(answer(&root, "expired", "done").is_err());
        assert_eq!(
            execute(
                &root,
                "owner",
                "Mac",
                Request::Poll {
                    id: "expired".into()
                }
            )
            .unwrap()["state"],
            "expired"
        );
        assert!(pending(&root).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
