//! Что устройство помнит о себе: ключ (`<dir>/key`) и `<dir>/device.json` — имя, роль и
//! связанные устройства.

use std::fs;
use std::io;
use std::path::Path;

use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};

use crate::proto::Role;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Store {
    pub name: String,
    #[serde(default)]
    pub role: Role,
    #[serde(default)]
    pub peers: Vec<Peer>,
}

/// Связанное устройство: доверие взаимное, имя — последнее, что оно о себе сообщило.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Peer {
    pub id: EndpointId,
    pub name: String,
}

impl Store {
    pub fn load(dir: &Path) -> io::Result<Store> {
        match fs::read_to_string(dir.join("device.json")) {
            Ok(text) => serde_json::from_str(&text).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Store {
                name: default_name(),
                role: Role::default(),
                peers: Vec::new(),
            }),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        ferry_core::write_private(&dir.join("device.json"), &(text + "\n"))
    }

    pub fn peer(&self, id: EndpointId) -> Option<&Peer> {
        self.peers.iter().find(|p| p.id == id)
    }
}

/// Имя компьютера без `.local` — так его зовут и в сети.
fn default_name() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();
    let name = name.strip_suffix(".local").unwrap_or(&name);
    if name.is_empty() {
        "ferry".into()
    } else {
        name.into()
    }
}

/// Имя устройства: непустое, без переводов строк, не длиннее 64 символов.
pub fn valid_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.contains(['\n', '\r']) {
        return Err("имя — от 1 до 64 символов, в одну строку".into());
    }
    Ok(name.into())
}

/// Ключ устройства: создаётся при первом запуске, лежит только у владельца.
pub fn key(dir: &Path) -> io::Result<SecretKey> {
    let path = dir.join("key");
    match fs::read_to_string(&path) {
        Ok(hex) => {
            let bytes = decode_hex(hex.trim())
                .ok_or_else(|| io::Error::other(format!("{} испорчен", path.display())))?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(dir)?;
            let key = SecretKey::generate();
            let hex: String = key.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
            ferry_core::write_private(&path, &(hex + "\n"))?;
            Ok(key)
        }
        Err(e) => Err(e),
    }
}

fn decode_hex(hex: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    if hex.len() != 64 {
        return None;
    }
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_and_store_survive_restart() {
        let dir = std::env::temp_dir().join(format!("ferry-store-{}", std::process::id()));
        let first = key(&dir).unwrap();
        assert_eq!(key(&dir).unwrap().public(), first.public());

        let mut store = Store::load(&dir).unwrap();
        assert!(!store.name.is_empty());
        store.role = Role::Node;
        store.peers.push(Peer {
            id: SecretKey::generate().public(),
            name: "win-10".into(),
        });
        store.save(&dir).unwrap();
        assert_eq!(Store::load(&dir).unwrap(), store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(valid_name("  win-10 ").unwrap(), "win-10");
        assert!(valid_name("").is_err());
        assert!(valid_name("a\nb").is_err());
        assert!(valid_name(&"я".repeat(65)).is_err());
    }
}
