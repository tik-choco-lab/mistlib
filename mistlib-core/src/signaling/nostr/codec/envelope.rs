use super::super::event::random_subscription_id;
use super::super::limits::MAX_NOSTR_SIGNALING_PLAINTEXT_BYTES;
use crate::error::{MistError, Result};
use crate::signaling::SignalingData;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct NostrMessageEnvelope {
    version: u8,
    message_id: String,
    sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    sender_joined_at: Option<u64>,
    data: SignalingData,
}

#[derive(Clone, Debug)]
pub(super) struct DecodedMessageEnvelope {
    pub(super) data: SignalingData,
    pub(super) message_id: Option<String>,
    pub(super) sequence: Option<u64>,
    pub(super) sender_joined_at: Option<u64>,
}

pub(super) fn encode_message_envelope_with_joined_at(
    data: &SignalingData,
    sequence: u64,
    sender_joined_at: Option<u64>,
) -> Result<Vec<u8>> {
    if sequence == 0 {
        return Err(MistError::Signaling(
            "Nostr message sequence must be positive".to_string(),
        ));
    }
    let envelope = NostrMessageEnvelope {
        version: 1,
        message_id: random_subscription_id(),
        sequence,
        sender_joined_at,
        data: data.clone(),
    };
    let encoded = serde_json::to_vec(&envelope)?;
    if encoded.len() > MAX_NOSTR_SIGNALING_PLAINTEXT_BYTES {
        return Err(MistError::Signaling(
            "Nostr signaling payload is too large".to_string(),
        ));
    }
    Ok(encoded)
}

pub(super) fn decode_message_envelope(plaintext: &[u8]) -> Result<DecodedMessageEnvelope> {
    if plaintext.len() > MAX_NOSTR_SIGNALING_PLAINTEXT_BYTES {
        return Err(MistError::Signaling(
            "Nostr signaling payload is too large".to_string(),
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(plaintext)?;
    let is_envelope = value.get("Version").is_some()
        || value.get("MessageId").is_some()
        || value.get("Sequence").is_some();
    if !is_envelope {
        let data = serde_json::from_value(value)?;
        return Ok(DecodedMessageEnvelope {
            data,
            message_id: None,
            sequence: None,
            sender_joined_at: None,
        });
    }

    let envelope: NostrMessageEnvelope = serde_json::from_value(value)?;
    if envelope.version != 1 {
        return Err(MistError::Signaling(
            "unsupported Nostr message envelope".to_string(),
        ));
    }
    if envelope.sequence == 0 {
        return Err(MistError::Signaling(
            "invalid Nostr message sequence".to_string(),
        ));
    }
    if envelope.message_id.len() != 32
        || !envelope.message_id.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(MistError::Signaling("invalid Nostr message id".to_string()));
    }
    Ok(DecodedMessageEnvelope {
        data: envelope.data,
        message_id: Some(envelope.message_id),
        sequence: Some(envelope.sequence),
        sender_joined_at: envelope.sender_joined_at,
    })
}
