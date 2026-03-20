use crate::types::NodeId;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SignalingType {
    Offer,
    Answer,
    Candidate,
    Candidates,
    Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SignalingData {
    pub sender_id: NodeId,
    pub receiver_id: NodeId,
    pub room_id: String,
    #[serde(default)]
    pub data: String,
    #[serde(rename = "Type")]
    pub signaling_type: SignalingType,
}

pub const OVERLAY_MSG_HEARTBEAT: u32 = 0;
pub const OVERLAY_MSG_REQUEST_NODE_LIST: u32 = 1;
pub const OVERLAY_MSG_NODE_LIST: u32 = 2;
pub const OVERLAY_MSG_PING: u32 = 3;
pub const OVERLAY_MSG_PONG: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayMessage {
    pub message_type: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageContent {
    Data(SignalingData),
    Overlay(OverlayMessage),
    Raw(Bytes),
}

impl From<Vec<u8>> for MessageContent {
    fn from(data: Vec<u8>) -> Self {
        MessageContent::Raw(Bytes::from(data))
    }
}

impl From<Bytes> for MessageContent {
    fn from(data: Bytes) -> Self {
        MessageContent::Raw(data)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalingEnvelope {
    pub from: NodeId,
    pub to: NodeId,
    pub hop_count: u32,
    pub content: MessageContent,
}
