use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct NodeId(pub String);

impl NodeId {
    pub const SERVER: &'static str = "server";
    pub const BROADCAST: &'static str = "";

    pub fn server() -> Self {
        Self(Self::SERVER.to_string())
    }

    pub fn broadcast() -> Self {
        Self(Self::BROADCAST.to_string())
    }

    pub fn is_server(&self) -> bool {
        self.0 == Self::SERVER
    }

    pub fn is_broadcast(&self) -> bool {
        self.0.is_empty()
    }
}

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
impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Disconnected => f.write_str("disconnected"),
            ConnectionState::Connecting => f.write_str("connecting"),
            ConnectionState::Connected => f.write_str("connected"),
            ConnectionState::Reconnecting => f.write_str("reconnecting"),
            ConnectionState::Failed => f.write_str("failed"),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub trait HostSendSync: Send + Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + Sync + ?Sized> HostSendSync for T {}

#[cfg(target_arch = "wasm32")]
pub trait HostSendSync {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> HostSendSync for T {}
