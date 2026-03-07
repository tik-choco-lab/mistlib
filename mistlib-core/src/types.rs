use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct NodeId(pub String);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum DeliveryMethod {
    ReliableOrdered = 0,
    UnreliableOrdered = 1,
    Unreliable = 2,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Failed,
}
#[cfg(not(target_arch = "wasm32"))]
pub trait HostSendSync: Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync + ?Sized> HostSendSync for T {}

#[cfg(target_arch = "wasm32")]
pub trait HostSendSync {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> HostSendSync for T {}
