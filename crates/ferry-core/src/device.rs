//! Устройства ferry: что они сообщают о себе и что видно с одного из них. Общие типы для
//! агента (`ferry-net`), приложения и CLI. Ключ устройства здесь — строкой (hex): так его
//! видят и человек, и JSON.

use serde::{Deserialize, Serialize};

use crate::mode::Mode;

/// Что устройство делает. Меняется с любого связанного устройства, в отличие от режима —
/// его выбирает только человек за самим устройством.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Управляет другими и принимает команды.
    #[default]
    Both,
    /// Принимает команды: на нём собирают и тестируют.
    Node,
    /// Только управляет другими.
    Console,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Both, Role::Node, Role::Console];

    pub fn name(self) -> &'static str {
        match self {
            Role::Both => "both",
            Role::Node => "node",
            Role::Console => "console",
        }
    }

    pub fn parse(name: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.name() == name)
    }

    pub fn label(self) -> &'static str {
        match self {
            Role::Both => "узел и пульт",
            Role::Node => "узел",
            Role::Console => "пульт",
        }
    }

    pub fn accepts_commands(self) -> bool {
        self != Role::Console
    }
}

/// Что устройство сообщает о себе.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Status {
    pub name: String,
    pub role: Role,
    pub mode: Mode,
    pub os: String,
    pub version: String,
}

/// Где на устройстве выполнять команду: в домашнем каталоге или в том же месте проекта, что
/// здесь (`~/ferry/<name>/<prefix>`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Dir {
    Home,
    Project { name: String, prefix: String },
}

/// Ошибка с кодом выхода для CLI: [`crate::mode::REFUSED`] (75) — устройство отказало по режиму
/// или роли, остальное — 1.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Fail {
    pub code: i32,
    pub message: String,
}

impl Fail {
    pub fn new(message: impl std::fmt::Display) -> Fail {
        Fail {
            code: 1,
            message: message.to_string(),
        }
    }

    pub fn refused(message: impl std::fmt::Display) -> Fail {
        Fail {
            code: crate::mode::REFUSED,
            message: message.to_string(),
        }
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Fail {}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Fail {
        Fail::new(e)
    }
}

/// Что видно с этого устройства: оно само, связанные устройства, устройства рядом.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub id: String,
    pub me: Status,
    pub peers: Vec<PeerView>,
    /// Устройства ferry в локальной сети, ещё не связанные с этим.
    pub lan: Vec<LanDevice>,
    pub pairing: Option<PairingInfo>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PeerView {
    pub id: String,
    pub name: String,
    pub online: bool,
    pub status: Option<Status>,
    /// Путь до устройства: напрямую или через relay; задержка по нему.
    pub direct: Option<bool>,
    pub rtt_ms: Option<u32>,
}

/// Устройство ferry, замеченное в локальной сети.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LanDevice {
    pub id: String,
    pub name: String,
    /// Ждёт связывания: показывает код.
    pub pairing: bool,
}

/// Открытое окно связывания на этом устройстве.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PairingInfo {
    pub code: String,
    /// Для связывания из другой сети.
    pub ticket: String,
    pub seconds_left: u64,
}

impl Snapshot {
    /// Связанное устройство по имени или началу ключа (от 4 знаков).
    pub fn find(&self, who: &str) -> Result<&PeerView, String> {
        if let Some(peer) = self.peers.iter().find(|p| p.name == who) {
            return Ok(peer);
        }
        let by_key: Vec<_> = if who.len() >= 4 {
            self.peers
                .iter()
                .filter(|p| p.id.starts_with(who))
                .collect()
        } else {
            Vec::new()
        };
        match by_key[..] {
            [peer] => Ok(peer),
            [] => {
                let names: Vec<_> = self.peers.iter().map(|p| p.name.as_str()).collect();
                Err(if names.is_empty() {
                    format!("нет устройства {who:?}: связанных устройств пока нет — ferry pair")
                } else {
                    format!("нет устройства {who:?}; связаны: {}", names.join(", "))
                })
            }
            _ => Err(format!(
                "{who:?} подходит нескольким устройствам — укажи имя"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, name: &str) -> PeerView {
        PeerView {
            id: id.into(),
            name: name.into(),
            online: true,
            status: None,
            direct: None,
            rtt_ms: None,
        }
    }

    #[test]
    fn devices_are_found_by_name_or_key_prefix() {
        let me = Status {
            name: "mac".into(),
            role: Role::Both,
            mode: Mode::Active,
            os: "macos".into(),
            version: "0.1.0".into(),
        };
        let snap = Snapshot {
            id: "ffff".into(),
            me,
            peers: vec![peer("c6a00fcf", "win-10"), peer("c6b1aaaa", "win-11")],
            lan: Vec::new(),
            pairing: None,
        };
        assert_eq!(snap.find("win-11").unwrap().id, "c6b1aaaa");
        assert_eq!(snap.find("c6a0").unwrap().name, "win-10");
        assert!(snap.find("c6").unwrap_err().contains("нет устройства"));
        assert!(snap.find("c6a").unwrap_err().contains("нет устройства"));
        assert!(snap.find("c6a0c6").is_err());
        let err = Snapshot {
            peers: vec![peer("c6a0aaaa", "a"), peer("c6a0bbbb", "b")],
            ..snap
        }
        .find("c6a0")
        .unwrap_err();
        assert!(err.contains("нескольким"), "{err}");
    }

    #[test]
    fn json_is_what_the_app_reads() {
        let status = Status {
            name: "win-10".into(),
            role: Role::Node,
            mode: Mode::NoDesktop,
            os: "windows".into(),
            version: "0.1.0".into(),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            json.contains(r#""role":"node""#) && json.contains(r#""mode":"no-desktop""#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), status);
    }
}
