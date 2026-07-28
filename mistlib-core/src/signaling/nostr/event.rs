use super::limits::MAX_NOSTR_RELAY_FRAME_BYTES;
use super::util::{hex_encode, now_unix_seconds, sha256_hex};
use crate::error::{MistError, Result};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NostrEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u32,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

impl NostrEvent {
    pub fn unsigned(pubkey: String, kind: u32, tags: Vec<Vec<String>>, content: String) -> Self {
        let mut event = Self {
            id: String::new(),
            pubkey,
            created_at: now_unix_seconds(),
            kind,
            tags,
            content,
            sig: String::new(),
        };
        event.refresh_id();
        event
    }

    pub fn refresh_id(&mut self) {
        let canonical = serde_json::json!([
            0,
            self.pubkey,
            self.created_at,
            self.kind,
            self.tags,
            self.content
        ]);
        let encoded = serde_json::to_vec(&canonical).unwrap_or_default();
        self.id = sha256_hex(&encoded);
    }

    pub fn tag_value(&self, name: &str) -> Option<&str> {
        self.tags.iter().find_map(|tag| {
            if tag.len() >= 2 && tag[0] == name {
                Some(tag[1].as_str())
            } else {
                None
            }
        })
    }

    pub fn has_tag_value(&self, name: &str, value: &str) -> bool {
        self.tags
            .iter()
            .any(|tag| tag.len() >= 2 && tag[0] == name && tag[1] == value)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NostrFilter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authors: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    #[serde(flatten)]
    pub tag_filters: BTreeMap<String, Vec<String>>,
}

impl NostrFilter {
    pub fn with_tag(mut self, name: &str, values: Vec<String>) -> Self {
        self.tag_filters.insert(format!("#{name}"), values);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayMessage {
    Event {
        subscription_id: String,
        event: NostrEvent,
    },
    Eose {
        subscription_id: String,
    },
    Ok {
        event_id: String,
        accepted: bool,
        message: String,
    },
    Notice(String),
    Closed {
        subscription_id: String,
        message: String,
    },
    Auth(String),
}

pub fn req_frame_json(subscription_id: &str, filters: &[NostrFilter]) -> Result<String> {
    let mut frame = Vec::with_capacity(filters.len() + 2);
    frame.push(serde_json::Value::String("REQ".to_string()));
    frame.push(serde_json::Value::String(subscription_id.to_string()));
    for filter in filters {
        frame.push(serde_json::to_value(filter)?);
    }
    serde_json::to_string(&frame).map_err(Into::into)
}

pub fn random_subscription_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

pub fn event_frame_json(event: &NostrEvent) -> Result<String> {
    serde_json::to_string(&serde_json::json!(["EVENT", event])).map_err(Into::into)
}

pub fn close_frame_json(subscription_id: &str) -> Result<String> {
    serde_json::to_string(&serde_json::json!(["CLOSE", subscription_id])).map_err(Into::into)
}

pub fn parse_relay_message(raw: &str) -> Result<Option<RelayMessage>> {
    if raw.len() > MAX_NOSTR_RELAY_FRAME_BYTES {
        return Err(MistError::Signaling(
            "Nostr relay frame is too large".to_string(),
        ));
    }
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let Some(items) = value.as_array() else {
        return Err(MistError::Signaling(
            "relay frame must be a JSON array".to_string(),
        ));
    };
    let Some(command) = items.first().and_then(serde_json::Value::as_str) else {
        return Err(MistError::Signaling(
            "relay frame command must be a string".to_string(),
        ));
    };

    match command {
        "EVENT" if items.len() >= 3 => {
            let subscription_id = relay_frame_string(items, 1, "EVENT sub id")?;
            let event = serde_json::from_value::<NostrEvent>(items[2].clone())?;
            Ok(Some(RelayMessage::Event {
                subscription_id,
                event,
            }))
        }
        "EOSE" if items.len() >= 2 => Ok(Some(RelayMessage::Eose {
            subscription_id: relay_frame_string(items, 1, "EOSE sub id")?,
        })),
        "OK" if items.len() >= 4 => Ok(Some(RelayMessage::Ok {
            event_id: relay_frame_string(items, 1, "OK event id")?,
            accepted: relay_frame_bool(items, 2, "OK accepted")?,
            message: relay_frame_string(items, 3, "OK message")?,
        })),
        "NOTICE" if items.len() >= 2 => Ok(Some(RelayMessage::Notice(relay_frame_string(
            items,
            1,
            "NOTICE message",
        )?))),
        "CLOSED" if items.len() >= 3 => Ok(Some(RelayMessage::Closed {
            subscription_id: relay_frame_string(items, 1, "CLOSED sub id")?,
            message: relay_frame_string(items, 2, "CLOSED message")?,
        })),
        "AUTH" if items.len() >= 2 => Ok(Some(RelayMessage::Auth(relay_frame_string(
            items,
            1,
            "AUTH challenge",
        )?))),
        _ => Ok(None),
    }
}

fn relay_frame_string(
    items: &[serde_json::Value],
    index: usize,
    field_name: &str,
) -> Result<String> {
    items
        .get(index)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| MistError::Signaling(format!("{field_name} must be string")))
}

fn relay_frame_bool(items: &[serde_json::Value], index: usize, field_name: &str) -> Result<bool> {
    items
        .get(index)
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| MistError::Signaling(format!("{field_name} must be bool")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_relay_frame_is_rejected_before_json_parse() {
        let raw = format!(
            "[\"NOTICE\",\"{}\"]",
            "x".repeat(MAX_NOSTR_RELAY_FRAME_BYTES)
        );

        assert!(parse_relay_message(&raw).is_err());
    }

    #[test]
    fn parses_relay_auth_and_closed_frames() {
        let auth = parse_relay_message(r#"["AUTH","challenge"]"#)
            .unwrap()
            .unwrap();
        assert_eq!(auth, RelayMessage::Auth("challenge".to_string()));

        let closed = parse_relay_message(r#"["CLOSED","sub","auth-required: restricted"]"#)
            .unwrap()
            .unwrap();
        assert_eq!(
            closed,
            RelayMessage::Closed {
                subscription_id: "sub".to_string(),
                message: "auth-required: restricted".to_string(),
            }
        );
    }
}
