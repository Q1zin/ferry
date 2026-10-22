//! Explicit agent lifetime, independent from a particular agent executable.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Begin {
        id: String,
        owner_pid: Option<u32>,
        ttl: u32,
    },
    Renew {
        id: String,
        ttl: u32,
    },
    End {
        id: String,
    },
    Show {
        id: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Owner {
    id: String,
    owner_pid: Option<u32>,
    birth: Option<String>,
    expires: u64,
    ended: bool,
}
fn path(root: &Path, id: &str) -> io::Result<PathBuf> {
    if id.is_empty()
        || id.len() > 100
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
    {
        return Err(io::Error::other(
            "session id: буквы ASCII, цифры, _ и - (1–100)",
        ));
    }
    Ok(root.join("sessions").join(format!("{id}.json")))
}
fn ttl(seconds: u32) -> io::Result<u64> {
    if !(10..=86400).contains(&seconds) {
        return Err(io::Error::other("session ttl: 10–86400 секунд"));
    }
    Ok(crate::now() + u64::from(seconds))
}
pub fn execute(root: &Path, request: Request) -> io::Result<Value> {
    let id = match &request {
        Request::Begin { id, .. }
        | Request::Renew { id, .. }
        | Request::End { id }
        | Request::Show { id } => id,
    };
    let file = path(root, id)?;
    fs::create_dir_all(file.parent().unwrap())?;
    let _lock = crate::workspace::lock(root, &format!("session-{id}"))?;
    let owner = match request {
        Request::Begin {
            id,
            owner_pid,
            ttl: seconds,
        } => {
            if file.exists() {
                return Err(io::Error::other("session id уже использован; выбери новый"));
            }
            let birth = owner_pid.map(process_birth).transpose()?;
            Owner {
                id,
                owner_pid,
                birth,
                expires: ttl(seconds)?,
                ended: false,
            }
        }
        Request::Renew { ttl: seconds, .. } => {
            let mut owner: Owner = serde_json::from_slice(&fs::read(&file)?)?;
            if ended_owner(&owner)? {
                return Err(io::Error::other("сессия уже закончилась"));
            }
            owner.expires = ttl(seconds)?;
            owner
        }
        Request::End { .. } => {
            let mut owner: Owner = serde_json::from_slice(&fs::read(&file)?)?;
            owner.ended = true;
            owner
        }
        Request::Show { .. } => serde_json::from_slice(&fs::read(&file)?)?,
    };
    crate::write_private(&file, &serde_json::to_string(&owner)?)?;
    Ok(
        json!({"session":format!("agent:{}",owner.id),"owner_pid":owner.owner_pid,"expires":owner.expires,"ended":ended_owner(&owner)?}),
    )
}
fn ended_owner(owner: &Owner) -> io::Result<bool> {
    if owner.ended || owner.expires <= crate::now() {
        return Ok(true);
    }
    if let Some(pid) = owner.owner_pid {
        return match process_birth(pid) {
            Ok(birth) => Ok(Some(birth) != owner.birth),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e),
        };
    }
    Ok(false)
}
pub fn managed(name: &str) -> bool {
    name.starts_with("agent:") || crate::control::session_pid(name).is_some()
}
pub fn ended(root: &Path, name: &str) -> io::Result<bool> {
    let Some(id) = name.strip_prefix("agent:") else {
        return Ok(false);
    };
    let owner: Owner = serde_json::from_slice(&fs::read(path(root, id)?)?)?;
    ended_owner(&owner)
}
#[cfg(unix)]
fn process_birth(pid: u32) -> io::Result<String> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "процесс владельца не существует",
        ));
    }
    Ok(value)
}
#[cfg(windows)]
fn process_birth(pid: u32) -> io::Result<String> {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn GetProcessTimes(
            handle: *mut c_void,
            creation: *mut u64,
            exit: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    unsafe {
        let h = OpenProcess(0x1000, 0, pid);
        if h.is_null() {
            let e = io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(87) {
                io::Error::new(io::ErrorKind::NotFound, "процесс владельца не существует")
            } else {
                e
            });
        }
        let (mut creation, mut exit, mut kernel, mut user) = (0, 0, 0, 0);
        let ok = GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user);
        let error = io::Error::last_os_error();
        CloseHandle(h);
        if ok == 0 {
            Err(error)
        } else if exit != 0 {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "процесс владельца завершился",
            ))
        } else {
            Ok(creation.to_string())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ended_session_cannot_be_reused() {
        let root = std::env::temp_dir().join(format!("ferry-session-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let begin = Request::Begin {
            id: "agent-test".into(),
            owner_pid: Some(std::process::id()),
            ttl: 60,
        };
        execute(&root, begin.clone()).unwrap();
        assert!(!ended(&root, "agent:agent-test").unwrap());
        execute(
            &root,
            Request::End {
                id: "agent-test".into(),
            },
        )
        .unwrap();
        assert!(ended(&root, "agent:agent-test").unwrap());
        assert!(execute(&root, begin).is_err());
        assert!(
            execute(
                &root,
                Request::Renew {
                    id: "agent-test".into(),
                    ttl: 60
                }
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
