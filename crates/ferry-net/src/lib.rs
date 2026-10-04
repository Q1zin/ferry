//! Связь устройств ferry поверх iroh: ключ устройства, поиск в локальной сети, связывание по
//! коду и агент, который держит связь со всеми связанными устройствами. Каждое устройство
//! адресуется своим ключом, а не IP: сменился адрес, VPN, другая сеть — связь та же.

pub mod pair;
pub mod proto;
pub mod store;

pub use ferry_core::mode::Mode;
pub use iroh::EndpointId;
pub use proto::{Role, Status};
