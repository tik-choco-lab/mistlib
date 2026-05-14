use crate::signaling::MessageContent;
use crate::types::NodeId;
use serde::{Deserialize, Serialize};

pub const OVERLAY_MSG_HEARTBEAT: u32 = 0;
pub const OVERLAY_MSG_REQUEST_NODE_LIST: u32 = 1;
pub const OVERLAY_MSG_NODE_LIST: u32 = 2;
pub const OVERLAY_MSG_PING: u32 = 3;
pub const OVERLAY_MSG_PONG: u32 = 4;
/// 旧 NodeList mode の位置更新メッセージ。現在の送信側は self を含む NodeList を使う。
pub const OVERLAY_MSG_POSITION: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayMessage {
    pub message_type: u32,
    pub payload: Vec<u8>,
}

impl OverlayMessage {
    pub fn is_internal_control(&self) -> bool {
        matches!(
            self.message_type,
            OVERLAY_MSG_HEARTBEAT
                | OVERLAY_MSG_REQUEST_NODE_LIST
                | OVERLAY_MSG_NODE_LIST
                | OVERLAY_MSG_PING
                | OVERLAY_MSG_PONG
                | OVERLAY_MSG_POSITION
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayEnvelope {
    pub from: NodeId,
    pub to: NodeId,
    pub hop_count: u32,
    pub content: MessageContent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dnve3_control_messages_are_internal() {
        for message_type in [
            OVERLAY_MSG_HEARTBEAT,
            OVERLAY_MSG_REQUEST_NODE_LIST,
            OVERLAY_MSG_NODE_LIST,
            OVERLAY_MSG_PING,
            OVERLAY_MSG_PONG,
            OVERLAY_MSG_POSITION,
        ] {
            assert!(OverlayMessage {
                message_type,
                payload: Vec::new(),
            }
            .is_internal_control());
        }
    }

    #[test]
    fn application_overlay_messages_are_not_internal() {
        assert!(!OverlayMessage {
            message_type: 100,
            payload: Vec::new(),
        }
        .is_internal_control());
    }
}
