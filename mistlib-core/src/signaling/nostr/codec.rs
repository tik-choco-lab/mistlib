use super::crypto::NostrCrypto;
use super::event::{random_subscription_id, NostrEvent, NostrFilter};
use super::identity::TemporarySignalingIdentity;
use super::invite::{
    derive_broadcast_sentinel, derive_discovery_proof, derive_invite_scope, derive_invite_secret,
    derive_room_scope, derive_topology_rank,
};
use super::util::now_unix_seconds;
use crate::config::NostrSignalingConfig;
use crate::error::{MistError, Result};
use crate::signaling::SignalingData;
use crate::types::NodeId;

mod envelope;
mod validation;

use envelope::{decode_message_envelope, encode_message_envelope_with_joined_at};
use validation::{
    event_expiration, validate_discovery_event, validate_event_basics, validate_event_shape,
};
pub use validation::{is_broadcast_sentinel_message, is_room_mailbox_message};

pub const TAG_INVITE_SCOPE: &str = "d";
pub const TAG_EXPIRATION: &str = "expiration";
pub const TAG_NONCE: &str = "nonce";
pub const TAG_DISCOVERY_PROOF: &str = "proof";
pub const TAG_JOINED_AT: &str = "joined_at";
pub const TAG_P: &str = "p";
const MIN_ROOM_SCOPE_ROTATION_SECONDS: u64 = 30;
const MAX_ROOM_SCOPE_ROTATION_SECONDS: u64 = 3_600;
/// Default clock-skew tolerance used when [`NostrSignalingConfig::max_clock_skew_seconds`]
/// is not overridden. Kept here purely as the default value definition; the value actually
/// enforced during validation always comes from [`NostrCodecConfig::max_clock_skew_seconds`].
pub const DEFAULT_MAX_EVENT_CLOCK_SKEW_SECONDS: u64 = 300;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NostrCodecConfig {
    pub discovery_kind: u32,
    pub message_kind: u32,
    pub ttl_seconds: u64,
    /// Clock-skew tolerance (seconds) applied when validating `created_at` / `expiration`
    /// on received events. See [`NostrSignalingConfig::max_clock_skew_seconds`] for the
    /// wire-compatibility notes (this is a local acceptance-window setting only).
    pub max_clock_skew_seconds: u64,
    pub invite_scope: String,
    room_scope_secret: [u8; 32],
}

impl NostrCodecConfig {
    pub fn from_config(config: &NostrSignalingConfig) -> Self {
        Self {
            discovery_kind: config.discovery_kind,
            message_kind: config.message_kind,
            ttl_seconds: config.ttl_seconds,
            max_clock_skew_seconds: config.max_clock_skew_seconds,
            invite_scope: derive_invite_scope(&config.invite_salt, &config.invite_code),
            room_scope_secret: derive_invite_secret(&config.invite_salt, &config.invite_code),
        }
    }

    pub fn expires_at(&self) -> u64 {
        now_unix_seconds().saturating_add(self.ttl_seconds)
    }

    fn room_scope(&self, room_id: &str, rotation_bucket: u64) -> String {
        derive_room_scope(&self.room_scope_secret, room_id, rotation_bucket)
    }

    fn current_room_scope(&self, room_id: &str) -> String {
        self.room_scope(
            room_id,
            current_rotation_bucket(self.room_scope_rotation_seconds()),
        )
    }

    pub fn topology_rank(&self, room_id: &str, pubkey: &str) -> String {
        derive_topology_rank(&self.room_scope_secret, room_id, pubkey)
    }

    fn discovery_proof(&self, room_id: &str, pubkey: &str, expires_at: u64, nonce: &str) -> String {
        derive_discovery_proof(&self.room_scope_secret, room_id, pubkey, expires_at, nonce)
    }

    fn accepted_room_scopes(&self, room_id: &str) -> Vec<String> {
        let bucket = current_rotation_bucket(self.room_scope_rotation_seconds());
        let mut scopes = Vec::with_capacity(3);
        if bucket > 0 {
            scopes.push(self.room_scope(room_id, bucket - 1));
        }
        scopes.push(self.room_scope(room_id, bucket));
        scopes.push(self.room_scope(room_id, bucket + 1));
        scopes
    }

    fn broadcast_sentinel(&self, room_id: &str, rotation_bucket: u64) -> String {
        derive_broadcast_sentinel(&self.room_scope_secret, room_id, rotation_bucket)
    }

    /// The `p` tag value used on a freshly built kind-25050 message whose
    /// logical receiver is [`NodeId::broadcast`]. Identical for every member
    /// of the room in the current rotation bucket, so any member's
    /// `message_filter` subscription accepts it without revealing a real
    /// signaling pubkey.
    ///
    /// [`NodeId::broadcast`]: crate::types::NodeId::broadcast
    fn current_broadcast_sentinel(&self, room_id: &str) -> String {
        self.broadcast_sentinel(
            room_id,
            current_rotation_bucket(self.room_scope_rotation_seconds()),
        )
    }

    /// The broadcast sentinel window accepted when validating an incoming
    /// message and when subscribing (`#p` filter). Mirrors
    /// [`accepted_room_scopes`](Self::accepted_room_scopes)'s +/-1 rotation
    /// bucket window so a subscriber does not miss broadcast messages sent
    /// just before/after its own rotation boundary.
    fn accepted_broadcast_sentinels(&self, room_id: &str) -> Vec<String> {
        let bucket = current_rotation_bucket(self.room_scope_rotation_seconds());
        let mut sentinels = Vec::with_capacity(3);
        if bucket > 0 {
            sentinels.push(self.broadcast_sentinel(room_id, bucket - 1));
        }
        sentinels.push(self.broadcast_sentinel(room_id, bucket));
        sentinels.push(self.broadcast_sentinel(room_id, bucket + 1));
        sentinels
    }

    /// Interval at which the room scope ("d" tag) rotates. Subscriptions
    /// embed a static scope window, so subscribers must re-issue REQ filters
    /// at least once per rotation to keep receiving events from new peers.
    pub fn room_scope_rotation_seconds(&self) -> u64 {
        self.ttl_seconds.clamp(
            MIN_ROOM_SCOPE_ROTATION_SECONDS,
            MAX_ROOM_SCOPE_ROTATION_SECONDS,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedDiscovery {
    pub signaling_pubkey: String,
    pub expires_at: u64,
    pub topology_rank: String,
    pub joined_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedMessage {
    pub sender_pubkey: String,
    pub data: SignalingData,
    pub expires_at: u64,
    pub message_id: Option<String>,
    pub sequence: Option<u64>,
    pub sender_joined_at: Option<u64>,
}

pub fn discovery_filter(config: &NostrCodecConfig, room_id: &str) -> NostrFilter {
    NostrFilter {
        kinds: Some(vec![config.discovery_kind]),
        since: Some(now_unix_seconds().saturating_sub(config.ttl_seconds)),
        ..NostrFilter::default()
    }
    .with_tag(TAG_INVITE_SCOPE, config.accepted_room_scopes(room_id))
}

/// Builds the kind-25050 subscription filter for `local_pubkey`.
///
/// In addition to the existing `#d` room-scope window, the filter now also
/// constrains on `#p` so the relay only has to deliver messages this node can
/// actually consume: either directed at it by pubkey, or tagged with the
/// room's broadcast sentinel (see `NostrCodecConfig::accepted_broadcast_sentinels`).
/// Nostr filter tag-value lists are OR semantics, so a single filter catches
/// both directed and broadcast messages. This is what turns kind-25050
/// fan-out from O(room-size) into O(1) per message.
pub fn message_filter(config: &NostrCodecConfig, room_id: &str, local_pubkey: &str) -> NostrFilter {
    let mut p_values = Vec::with_capacity(4);
    p_values.push(local_pubkey.to_string());
    p_values.extend(config.accepted_broadcast_sentinels(room_id));
    NostrFilter {
        kinds: Some(vec![config.message_kind]),
        since: Some(now_unix_seconds().saturating_sub(config.ttl_seconds)),
        ..NostrFilter::default()
    }
    .with_tag(TAG_INVITE_SCOPE, config.accepted_room_scopes(room_id))
    .with_tag(TAG_P, p_values)
}

pub fn build_discovery_event<C: NostrCrypto>(
    config: &NostrCodecConfig,
    crypto: &C,
    identity: &TemporarySignalingIdentity,
    room_id: &str,
) -> Result<NostrEvent> {
    build_discovery_event_with_joined_at(config, crypto, identity, room_id, None)
}

pub fn build_discovery_event_with_joined_at<C: NostrCrypto>(
    config: &NostrCodecConfig,
    crypto: &C,
    identity: &TemporarySignalingIdentity,
    room_id: &str,
    joined_at: Option<u64>,
) -> Result<NostrEvent> {
    let expires_at = config.expires_at();
    let room_scope = config.current_room_scope(room_id);
    let nonce = random_subscription_id();
    let proof = config.discovery_proof(room_id, &identity.public_key, expires_at, &nonce);
    let mut tags = vec![
        tag(TAG_INVITE_SCOPE, &room_scope),
        tag(TAG_NONCE, &nonce),
        tag(TAG_EXPIRATION, &expires_at.to_string()),
        tag(TAG_DISCOVERY_PROOF, &proof),
    ];
    if let Some(joined_at) = joined_at {
        tags.push(tag(TAG_JOINED_AT, &joined_at.to_string()));
    }
    let mut event = NostrEvent::unsigned(
        identity.public_key.clone(),
        config.discovery_kind,
        tags,
        String::new(),
    );
    event.sig = crypto.sign_event(identity, &event)?;
    Ok(event)
}

pub fn decode_discovery_event<C: NostrCrypto>(
    config: &NostrCodecConfig,
    crypto: &C,
    event: &NostrEvent,
    room_id: &str,
) -> Result<DecodedDiscovery> {
    validate_event_shape(event)?;
    crypto.verify_event(event)?;
    validate_discovery_event(config, event, room_id)?;
    let expires_at = event_expiration(event)?;
    let joined_at = event
        .tag_value(TAG_JOINED_AT)
        .and_then(|value| value.parse::<u64>().ok());
    Ok(DecodedDiscovery {
        signaling_pubkey: event.pubkey.clone(),
        expires_at,
        topology_rank: config.topology_rank(room_id, &event.pubkey),
        joined_at,
    })
}

pub fn build_message_event<C: NostrCrypto>(
    config: &NostrCodecConfig,
    crypto: &C,
    identity: &TemporarySignalingIdentity,
    receiver_pubkey: &str,
    data: &SignalingData,
) -> Result<NostrEvent> {
    build_message_event_with_sequence(config, crypto, identity, receiver_pubkey, data, 1)
}

pub fn build_message_event_with_sequence<C: NostrCrypto>(
    config: &NostrCodecConfig,
    crypto: &C,
    identity: &TemporarySignalingIdentity,
    receiver_pubkey: &str,
    data: &SignalingData,
    sequence: u64,
) -> Result<NostrEvent> {
    build_message_event_with_sequence_and_joined_at(
        config,
        crypto,
        identity,
        receiver_pubkey,
        data,
        sequence,
        None,
    )
}

pub fn build_message_event_with_sequence_and_joined_at<C: NostrCrypto>(
    config: &NostrCodecConfig,
    crypto: &C,
    identity: &TemporarySignalingIdentity,
    receiver_pubkey: &str,
    data: &SignalingData,
    sequence: u64,
    sender_joined_at: Option<u64>,
) -> Result<NostrEvent> {
    let plaintext = encode_message_envelope_with_joined_at(data, sequence, sender_joined_at)?;
    let content = crypto.encrypt(identity, receiver_pubkey, &plaintext)?;
    let expires_at = config.expires_at();
    let message_scope = config.current_room_scope(&data.room_id);
    // Standard Nostr `p` tag targeting: a known receiver gets its real
    // signaling pubkey (relay-visible recipient targeting is the intentional
    // tradeoff that buys O(1) fan-out); a logical broadcast (receiver not yet
    // known as a NodeId) gets the room's rotating broadcast sentinel instead,
    // so it still reaches every current member without revealing a pubkey.
    let p_tag_value = if data.receiver_id.is_broadcast() {
        config.current_broadcast_sentinel(&data.room_id)
    } else {
        receiver_pubkey.to_string()
    };
    let tags = vec![
        tag(TAG_INVITE_SCOPE, &message_scope),
        tag(TAG_P, &p_tag_value),
        tag(TAG_NONCE, &random_subscription_id()),
        tag(TAG_EXPIRATION, &expires_at.to_string()),
    ];
    let mut event = NostrEvent::unsigned(
        identity.public_key.clone(),
        config.message_kind,
        tags,
        content,
    );
    event.sig = crypto.sign_event(identity, &event)?;
    Ok(event)
}

pub fn decode_message_event<C: NostrCrypto>(
    config: &NostrCodecConfig,
    crypto: &C,
    identity: &TemporarySignalingIdentity,
    local_node_id: &NodeId,
    event: &NostrEvent,
    room_id: &str,
) -> Result<DecodedMessage> {
    validate_event_shape(event)?;
    crypto.verify_event(event)?;
    validate_event_basics(config, config.message_kind, event)?;
    // A `p` tag is optional on decode (legacy senders that predate this
    // scheme, or a genuine room-mailbox message, omit it entirely) but when
    // present it must name either this receiver's own pubkey or the room's
    // current/adjacent broadcast sentinel — anything else means the event
    // was addressed to a different peer and a correctly filtering relay
    // should never have delivered it to us in the first place.
    match event.tag_value(TAG_P) {
        Some(pubkey)
            if pubkey == identity.public_key
                || config
                    .accepted_broadcast_sentinels(room_id)
                    .iter()
                    .any(|sentinel| sentinel == pubkey) =>
        {
            if !config
                .accepted_room_scopes(room_id)
                .iter()
                .any(|scope| event.has_tag_value(TAG_INVITE_SCOPE, scope))
            {
                return Err(MistError::Signaling(
                    "Nostr message room scope mismatch".to_string(),
                ));
            }
        }
        Some(_) => {
            return Err(MistError::Signaling(
                "Nostr message receiver pubkey mismatch".to_string(),
            ))
        }
        None if is_room_mailbox_message(config, event, room_id) => {}
        None => {
            return Err(MistError::Signaling(
                "Nostr message room scope mismatch".to_string(),
            ))
        }
    }
    let plaintext = crypto.decrypt(identity, &event.pubkey, &event.content)?;
    let decoded = decode_message_envelope(&plaintext)?;
    if decoded.data.room_id != room_id {
        return Err(MistError::Signaling(
            "decrypted Nostr message room mismatch".to_string(),
        ));
    }
    if decoded.data.receiver_id != *local_node_id && !decoded.data.receiver_id.is_broadcast() {
        return Err(MistError::Signaling(
            "decrypted Nostr message receiver mismatch".to_string(),
        ));
    }
    let expires_at = event
        .tag_value(TAG_EXPIRATION)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(|| now_unix_seconds().saturating_add(config.ttl_seconds));
    Ok(DecodedMessage {
        sender_pubkey: event.pubkey.clone(),
        data: decoded.data,
        expires_at,
        message_id: decoded.message_id,
        sequence: decoded.sequence,
        sender_joined_at: decoded.sender_joined_at,
    })
}

fn tag(name: &str, value: &str) -> Vec<String> {
    vec![name.to_string(), value.to_string()]
}

fn current_rotation_bucket(rotation_seconds: u64) -> u64 {
    now_unix_seconds() / rotation_seconds.max(1)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod security_tests;
