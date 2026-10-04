//! Протокол между связанными устройствами (irpc поверх iroh). Статус не опрашивают: `Watch`
//! присылает его сразу и потом каждое изменение.

use ferry_core::mode::Mode;
use iroh::endpoint::Connection;
use irpc::{
    Client,
    channel::{mpsc, oneshot},
    rpc_requests,
};
use irpc_iroh::IrohRemoteConnection;
use serde::{Deserialize, Serialize};

pub const ALPN: &[u8] = b"ferry/device/1";

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
    #[serde(with = "mode_name")]
    pub mode: Mode,
    pub os: String,
    pub version: String,
}

/// Режим — по имени (`active`, `no-desktop`, `paused`): ядро обходится без serde.
mod mode_name {
    use ferry_core::mode::Mode;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(mode: &Mode, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(mode.name())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Mode, D::Error> {
        let name = String::deserialize(d)?;
        Mode::parse(&name).ok_or_else(|| D::Error::custom(format!("режим {name:?}")))
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Watch;

#[derive(Serialize, Deserialize, Debug)]
pub struct SetRole(pub Role);

#[derive(Serialize, Deserialize, Debug)]
pub struct Rename(pub String);

#[rpc_requests(message = DeviceMessage)]
#[derive(Serialize, Deserialize, Debug)]
pub enum DeviceProtocol {
    /// Статус сразу и затем каждое его изменение.
    #[rpc(tx = mpsc::Sender<Status>)]
    Watch(Watch),
    #[rpc(tx = oneshot::Sender<Result<(), String>>)]
    SetRole(SetRole),
    #[rpc(tx = oneshot::Sender<Result<(), String>>)]
    Rename(Rename),
}

/// Клиент протокола поверх одного соединения с устройством.
#[derive(Clone, Debug)]
pub struct DeviceClient(Client<DeviceProtocol>);

impl DeviceClient {
    pub fn new(conn: Connection) -> Self {
        Self(Client::boxed(IrohRemoteConnection::new(conn)))
    }

    pub async fn watch(&self) -> irpc::Result<mpsc::Receiver<Status>> {
        self.0.server_streaming(Watch, 4).await
    }

    pub async fn set_role(&self, role: Role) -> anyhow::Result<()> {
        self.0.rpc(SetRole(role)).await?.map_err(anyhow::Error::msg)
    }

    pub async fn rename(&self, name: String) -> anyhow::Result<()> {
        self.0.rpc(Rename(name)).await?.map_err(anyhow::Error::msg)
    }
}
