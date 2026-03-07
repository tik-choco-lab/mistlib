use mistlib_core::types::NodeId;
use thiserror::Error;
use webrtc::Error as WebRtcError;

#[derive(Error, Debug)]
pub enum MistError {
    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("WebRTC error: {0}")]
    WebRtc(#[from] WebRtcError),

    #[error("Signaling error: {0}")]
    Signaling(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Bincode error: {0}")]
    Bincode(#[from] bincode::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Node not found: {0:?}")]
    NodeNotFound(NodeId),

    #[error("Route not found: {0:?}")]
    RouteNotFound(NodeId),

    #[error("Core error: {0}")]
    Core(#[from] mistlib_core::error::MistError),
}

pub type Result<T> = std::result::Result<T, MistError>;

impl From<String> for MistError {
    fn from(s: String) -> Self {
        MistError::Internal(s)
    }
}
