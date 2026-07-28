use super::super::event::NostrEvent;
use super::super::limits::{
    MAX_NOSTR_EVENT_CONTENT_CHARS, MAX_NOSTR_EVENT_TAGS, MAX_NOSTR_EVENT_TAG_FIELDS,
    MAX_NOSTR_EVENT_TAG_FIELD_CHARS,
};
use super::super::util::now_unix_seconds;
use super::{
    NostrCodecConfig, TAG_DISCOVERY_PROOF, TAG_EXPIRATION, TAG_INVITE_SCOPE, TAG_NONCE, TAG_P,
};
use crate::error::{MistError, Result};

pub fn is_room_mailbox_message(
    config: &NostrCodecConfig,
    event: &NostrEvent,
    room_id: &str,
) -> bool {
    event.kind == config.message_kind
        && event.tag_value(TAG_P).is_none()
        && config
            .accepted_room_scopes(room_id)
            .iter()
            .any(|scope| event.has_tag_value(TAG_INVITE_SCOPE, scope))
}

/// True when `event` is a kind-25050 message tagged with the room's shared
/// broadcast sentinel rather than a specific recipient pubkey — i.e. its
/// logical receiver was `NodeId::broadcast` at build time. Such messages are,
/// by design, still delivered to every room member (the sentinel is
/// identical for all of them), so — exactly like
/// [`is_room_mailbox_message`] for legacy p-tag-less events — a decrypt
/// failure for one of them means "not addressed to me", not a genuine error.
pub fn is_broadcast_sentinel_message(
    config: &NostrCodecConfig,
    event: &NostrEvent,
    room_id: &str,
) -> bool {
    event.kind == config.message_kind
        && event.tag_value(TAG_P).is_some_and(|pubkey| {
            config
                .accepted_broadcast_sentinels(room_id)
                .iter()
                .any(|sentinel| sentinel == pubkey)
        })
        && config
            .accepted_room_scopes(room_id)
            .iter()
            .any(|scope| event.has_tag_value(TAG_INVITE_SCOPE, scope))
}

pub(super) fn validate_discovery_event(
    config: &NostrCodecConfig,
    event: &NostrEvent,
    room_id: &str,
) -> Result<()> {
    validate_event_basics(config, config.discovery_kind, event)?;
    if !config
        .accepted_room_scopes(room_id)
        .iter()
        .any(|scope| event.has_tag_value(TAG_INVITE_SCOPE, scope))
    {
        return Err(MistError::Signaling(
            "Nostr invite scope mismatch".to_string(),
        ));
    }
    let nonce = event
        .tag_value(TAG_NONCE)
        .ok_or_else(|| MistError::Signaling("missing Nostr discovery nonce".to_string()))?;
    let proof = event
        .tag_value(TAG_DISCOVERY_PROOF)
        .ok_or_else(|| MistError::Signaling("missing Nostr discovery proof".to_string()))?;
    let expected = config.discovery_proof(room_id, &event.pubkey, event_expiration(event)?, nonce);
    if proof != expected {
        return Err(MistError::Signaling(
            "invalid Nostr discovery proof".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn validate_event_shape(event: &NostrEvent) -> Result<()> {
    if !is_hex_len(&event.id, 64) {
        return Err(MistError::Signaling("invalid Nostr event id".to_string()));
    }
    if !is_hex_len(&event.pubkey, 64) {
        return Err(MistError::Signaling("invalid Nostr pubkey".to_string()));
    }
    if !is_hex_len(&event.sig, 128) {
        return Err(MistError::Signaling("invalid Nostr signature".to_string()));
    }
    if event.content.len() > MAX_NOSTR_EVENT_CONTENT_CHARS {
        return Err(MistError::Signaling(
            "Nostr event content is too large".to_string(),
        ));
    }
    if event.tags.len() > MAX_NOSTR_EVENT_TAGS {
        return Err(MistError::Signaling("too many Nostr tags".to_string()));
    }
    for tag in &event.tags {
        if tag.len() > MAX_NOSTR_EVENT_TAG_FIELDS {
            return Err(MistError::Signaling(
                "too many Nostr tag fields".to_string(),
            ));
        }
        if tag
            .iter()
            .any(|field| field.len() > MAX_NOSTR_EVENT_TAG_FIELD_CHARS)
        {
            return Err(MistError::Signaling(
                "Nostr tag field is too large".to_string(),
            ));
        }
    }
    Ok(())
}

fn is_hex_len(value: &str, len: usize) -> bool {
    value.len() == len && value.chars().all(|c| c.is_ascii_hexdigit())
}

pub(super) fn validate_event_basics(
    config: &NostrCodecConfig,
    expected_kind: u32,
    event: &NostrEvent,
) -> Result<()> {
    validate_event_shape(event)?;
    if event.kind != expected_kind {
        return Err(MistError::Signaling(
            "unexpected Nostr event kind".to_string(),
        ));
    }
    let now = now_unix_seconds();
    if event.created_at > now.saturating_add(config.max_clock_skew_seconds) {
        return Err(MistError::Signaling(
            "Nostr event timestamp is too far in the future".to_string(),
        ));
    }
    if event
        .created_at
        .saturating_add(config.ttl_seconds)
        .saturating_add(config.max_clock_skew_seconds)
        < now
    {
        return Err(MistError::Signaling(
            "Nostr event timestamp is too old".to_string(),
        ));
    }
    let expires_at = event_expiration(event)?;
    if expires_at <= now {
        return Err(MistError::Signaling("expired Nostr event".to_string()));
    }
    if expires_at <= event.created_at {
        return Err(MistError::Signaling(
            "Nostr expiration must be after event timestamp".to_string(),
        ));
    }
    let max_expiration = now
        .saturating_add(config.ttl_seconds)
        .saturating_add(config.max_clock_skew_seconds);
    if expires_at > max_expiration {
        return Err(MistError::Signaling(
            "Nostr expiration is too far in the future".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn event_expiration(event: &NostrEvent) -> Result<u64> {
    event
        .tag_value(TAG_EXPIRATION)
        .ok_or_else(|| MistError::Signaling("missing Nostr expiration tag".to_string()))?
        .parse::<u64>()
        .map_err(|_| MistError::Signaling("invalid Nostr expiration tag".to_string()))
}
