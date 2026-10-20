//! Связь устройств ferry поверх iroh: ключ устройства, поиск в локальной сети, связывание по
//! коду и агент, который держит связь со всеми связанными устройствами. Каждое устройство
//! адресуется своим ключом, а не IP: сменился адрес, VPN, другая сеть — связь та же.

pub mod agent;
pub mod control;
pub mod desk;
pub mod device;
pub mod files;
pub mod lan;
mod node;
pub mod pair;
pub mod proto;
mod relay;
pub mod store;

pub use agent::{Agent, Options};
pub use device::Device;
pub use ferry_core::device::{
    Activity, Dir, Fail, LanDevice, PairingInfo, PeerView, Role, Snapshot, Status,
};
pub use ferry_core::mode::Mode;
pub use iroh::EndpointId;
