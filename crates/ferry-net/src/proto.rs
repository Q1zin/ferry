//! Протокол между связанными устройствами (irpc поверх iroh). Статус не опрашивают: `Watch`
//! присылает его сразу и потом каждое изменение.

use iroh::endpoint::Connection;
use irpc::{
    Client,
    channel::{mpsc, oneshot},
    rpc_requests,
};
use irpc_iroh::IrohRemoteConnection;
use serde::{Deserialize, Serialize};

pub const ALPN: &[u8] = b"ferry/device/1";

pub use ferry_core::device::{Role, Status};

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
