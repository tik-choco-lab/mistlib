use crate::overlay::OverlayMessage;
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
