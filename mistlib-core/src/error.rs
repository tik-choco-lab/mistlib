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

    #[error("message size {size} exceeds max_message_bytes {limit}")]
    MessageTooLarge { size: usize, limit: u32 },

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_too_large_display_includes_size_and_limit() {
        let err = MistError::MessageTooLarge {
            size: 100_000,
            limit: 65536,
        };
        let msg = err.to_string();
        assert_eq!(msg, "message size 100000 exceeds max_message_bytes 65536");
    }
}
