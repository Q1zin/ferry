//! Что устройство помнит о себе: ключ (`<dir>/key`) и `<dir>/device.json` — имя, роль и
//! связанные устройства.

use std::fs;
use std::io;
use std::path::Path;

use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};

use crate::proto::{Peers, Role};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Store {
    pub name: String,
    #[serde(default)]
    pub role: Role,
    #[serde(default)]
    pub peers: Vec<Peer>,
    /// Забытые устройства и когда забыли (секунды Unix): чтобы списки соседей их не вернули.
    #[serde(default)]
    pub forgotten: Vec<(EndpointId, u64)>,
}

/// Связанное устройство: доверие взаимное, имя — последнее, что оно о себе сообщило.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Peer {
    pub id: EndpointId,
    pub name: String,
    /// Когда связали (секунды Unix) — здесь или на любом из связанных; до 0.1.9 — 0.
    #[serde(default)]
    pub since: u64,
}

impl Store {
    pub fn load(dir: &Path) -> io::Result<Store> {
        match fs::read_to_string(dir.join("device.json")) {
            Ok(text) => serde_json::from_str(&text).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Store {
                name: default_name(),
                role: Role::default(),
                peers: Vec::new(),
                forgotten: Vec::new(),
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

    /// Связали в `at`: это отменяет более раннее «забыли».
    pub fn paired(&mut self, id: EndpointId, name: String, at: u64) {
        self.forgotten.retain(|(f, _)| *f != id);
        match self.peers.iter_mut().find(|p| p.id == id) {
            Some(peer) => {
                peer.name = name;
                peer.since = at;
            }
            None => self.peers.push(Peer {
                id,
                name,
                since: at,
            }),
        }
    }

    pub fn forget(&mut self, id: EndpointId, at: u64) {
        self.peers.retain(|p| p.id != id);
        self.forgotten.retain(|(f, _)| *f != id);
        self.forgotten.push((id, at));
    }

    /// Что рассказать связанным устройствам.
    pub fn shared(&self) -> Peers {
        Peers {
            known: self
                .peers
                .iter()
                .map(|p| (p.id, p.name.clone(), p.since))
                .collect(),
            forgotten: self.forgotten.clone(),
        }
    }

    /// Свести список соседа со своим: по каждому устройству побеждает то, что позже, — связали
    /// или забыли (в одну секунду — забыли). Себя не трогаем. `true` — что-то изменилось.
    pub fn merge(&mut self, me: EndpointId, theirs: &Peers) -> bool {
        let forgot = |store: &Store, id| {
            store
                .forgotten
                .iter()
                .find(|(f, _)| *f == id)
                .map(|(_, at)| *at)
        };
        let mut changed = false;
        for &(id, at) in &theirs.forgotten {
            let since = self.peer(id).map(|p| p.since);
            if id == me
                || since.is_some_and(|s| s > at)
                || forgot(self, id).is_some_and(|t| t >= at)
            {
                continue;
            }
            self.forget(id, at);
            changed = true;
        }
        for (id, name, since) in &theirs.known {
            let (id, since) = (*id, *since);
            if id == me || forgot(self, id).is_some_and(|t| t >= since) {
                continue;
            }
            match self.peers.iter_mut().find(|p| p.id == id) {
                Some(peer) if peer.since >= since => {}
                Some(peer) => {
                    peer.since = since;
                    changed = true;
                }
                None => {
                    self.paired(id, name.clone(), since);
                    changed = true;
                }
            }
        }
        changed
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
            since: 1,
        });
        store.save(&dir).unwrap();
        assert_eq!(Store::load(&dir).unwrap(), store);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_wins_when_merging_peer_lists() {
        let [me, c] = [(); 2].map(|_| SecretKey::generate().public());
        let mut store = Store {
            name: "a".into(),
            role: Role::default(),
            peers: Vec::new(),
            forgotten: Vec::new(),
        };
        let list = |known: Vec<(EndpointId, &str, u64)>, forgotten| Peers {
            known: known
                .into_iter()
                .map(|(id, name, at)| (id, name.into(), at))
                .collect(),
            forgotten,
        };
        let ids = |store: &Store| store.peers.iter().map(|p| p.id).collect::<Vec<_>>();

        // Сосед знакомит с c; себя из его списка не добавляем.
        let introduced = list(vec![(c, "c", 10), (me, "a", 10)], vec![]);
        assert!(store.merge(me, &introduced));
        assert_eq!(ids(&store), [c]);
        assert!(!store.merge(me, &introduced), "второй раз — ничего нового");

        // Забыли позже, чем связали, — забываем, и старый список c не вернёт.
        assert!(store.merge(me, &list(vec![], vec![(c, 20)])));
        assert!(store.peers.is_empty());
        assert!(!store.merge(me, &introduced));
        assert!(
            !store.merge(me, &list(vec![(c, "c", 20)], vec![])),
            "в ту же секунду — забыли"
        );

        // Связали снова — вернётся, и прежнее «забыли» эту связь не тронет.
        assert!(store.merge(me, &list(vec![(c, "c", 30)], vec![])));
        assert_eq!(ids(&store), [c]);
        assert!(store.forgotten.is_empty());
        assert!(!store.merge(me, &list(vec![], vec![(c, 20), (me, 40)])));
        assert_eq!(ids(&store), [c]);
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(valid_name("  win-10 ").unwrap(), "win-10");
        assert!(valid_name("").is_err());
        assert!(valid_name("a\nb").is_err());
        assert!(valid_name(&"я".repeat(65)).is_err());
    }
}
