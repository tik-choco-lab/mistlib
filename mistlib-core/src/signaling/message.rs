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
    /// A locally-synthesized notification, never published to a relay and
    /// never accepted from the wire. The signaling layer emits it into the
    /// local incoming stream when it detects that a peer's node id has
    /// rebound to a fresh signaling identity (i.e. the peer restarted). The
    /// transport uses it to tear down the stale peer connection before the
    /// peer's real Offer/Request arrives, because an abruptly-vanished
    /// WebRTC peer still reports `readyState == Open` locally for tens of
    /// seconds.
    Rejoin,
}

impl SignalingType {
    /// True only for [`SignalingType::Rejoin`]: variants that are
    /// synthesized locally by the signaling layer and must never be
    /// published to a relay (and are never expected to arrive from one).
    /// Publish paths should filter on this before sending a message out.
    pub fn is_local_only(&self) -> bool {
        matches!(self, SignalingType::Rejoin)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[cfg(test)]
mod tests {
    use super::SignalingType;

    #[test]
    fn only_rejoin_is_local_only() {
        assert!(SignalingType::Rejoin.is_local_only());
        assert!(!SignalingType::Offer.is_local_only());
        assert!(!SignalingType::Answer.is_local_only());
        assert!(!SignalingType::Candidate.is_local_only());
        assert!(!SignalingType::Candidates.is_local_only());
        assert!(!SignalingType::Request.is_local_only());
    }
}
