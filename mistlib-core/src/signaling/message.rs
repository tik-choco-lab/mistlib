use crate::types::NodeId;
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
    Raw(Vec<u8>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalingEnvelope {
    pub from: NodeId,
    pub to: NodeId,
    pub hop_count: u32,
    pub content: MessageContent,
}
