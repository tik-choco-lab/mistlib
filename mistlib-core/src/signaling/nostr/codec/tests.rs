use super::*;
use crate::config::NostrSignalingConfig;
use crate::signaling::nostr::{
    event_frame_json, random_subscription_id, req_frame_json, InvitePskCrypto, NostrCrypto,
    SignalingSecretKey,
};
use crate::signaling::SignalingType;

fn config() -> (NostrSignalingConfig, NostrCodecConfig, InvitePskCrypto) {
    let raw = NostrSignalingConfig {
        relays: vec!["ws://127.0.0.1:7777".to_string()],
        relay_list_url: None,
        discovery_kind: 25049,
        message_kind: 25050,
        ttl_seconds: 60,
        max_clock_skew_seconds: 300,
        invite_salt: "salt".to_string(),
        invite_code: "invite".to_string(),
    };
    let codec = NostrCodecConfig::from_config(&raw);
    let crypto = InvitePskCrypto::new(&raw.invite_salt, &raw.invite_code);
    (raw, codec, crypto)
}

mod privacy;
mod roundtrip;
mod validation;
