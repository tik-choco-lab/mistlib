use crate::types::NodeId;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MistError {
    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Signaling error: {0}")]
    Signaling(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Node not found: {0:?}")]
    NodeNotFound(NodeId),

    #[error("Route not found: {0:?}")]
    RouteNotFound(NodeId),

    #[error("Other error: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, MistError>;

impl From<String> for MistError {
    fn from(s: String) -> Self {
        MistError::Internal(s)
    }
}

impl From<serde_json::Error> for MistError {
    fn from(e: serde_json::Error) -> Self {
        MistError::Serialization(e.to_string())
    }
}

impl From<bincode::Error> for MistError {
    fn from(e: bincode::Error) -> Self {
        MistError::Serialization(e.to_string())
    }
}
