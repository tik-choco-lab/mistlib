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
    /// End-to-end acknowledgement for trickled ICE candidates. Unlike a
    /// relay `OK`, this proves that the intended peer received the payload.
    /// Kept at the end to preserve existing binary enum discriminants.
    CandidateAck,
    /// End-to-end acknowledgement for an Offer/Answer transaction.
    NegotiationAck,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CandidateEnvelope {
    #[serde(rename = "g")]
    pub generation: u32,
    /// Zero-based within `generation`; values 0..=63 are ACK-trackable.
    #[serde(rename = "s")]
    pub sequence: u8,
    #[serde(rename = "c")]
    pub candidate: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CandidateAck {
    #[serde(rename = "g")]
    pub generation: u32,
    /// Bit N acknowledges candidate sequence N.
    #[serde(rename = "m")]
    pub mask: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NegotiationEnvelope {
    /// Optional JSON-only capability; this DTO has no deny_unknown_fields.
    #[serde(
        rename = "reliableUnordered",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reliable_unordered: Option<u8>,
    #[serde(rename = "i")]
    pub id: u64,
    #[serde(rename = "s")]
    pub sdp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NegotiationAck {
    #[serde(rename = "i")]
    pub id: u64,
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
        assert!(!SignalingType::CandidateAck.is_local_only());
        assert!(!SignalingType::NegotiationAck.is_local_only());
        assert!(!SignalingType::Request.is_local_only());
    }
}

#[cfg(test)]
mod reliable_unordered_tests {
    use super::*;
    #[derive(Deserialize)]
    struct OldNegotiationEnvelope {
        i: u64,
        s: String,
    }
    #[test]
    fn optional_capability_keeps_default_json_and_old_reader_compatibility() {
        let plain = NegotiationEnvelope {
            id: 7,
            sdp: "sdp".into(),
            reliable_unordered: None,
        };
        let bytes = serde_json::to_string(&plain).unwrap();
        assert_eq!(bytes, r#"{"i":7,"s":"sdp"}"#);
        assert_eq!(
            serde_json::from_str::<NegotiationEnvelope>(&bytes).unwrap(),
            plain
        );
        let capable = NegotiationEnvelope {
            reliable_unordered: Some(1),
            ..plain
        };
        let bytes = serde_json::to_string(&capable).unwrap();
        let old: OldNegotiationEnvelope = serde_json::from_str(&bytes).unwrap();
        assert_eq!((old.i, old.s.as_str()), (7, "sdp"));
        assert_eq!(
            serde_json::from_str::<NegotiationEnvelope>(&bytes)
                .unwrap()
                .reliable_unordered,
            Some(1)
        );
    }
}
