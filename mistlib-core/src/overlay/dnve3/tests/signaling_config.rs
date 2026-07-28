use crate::config::{Config, SignalingMode};
use crate::signaling::nostr::DEFAULT_RELAY_LIST_URL;

#[test]
fn config_default_signaling_mode_is_nostr() {
    let config = Config::new_default();
    assert_eq!(config.signaling.mode, SignalingMode::Nostr);
    assert_eq!(config.signaling_url, "wss://rtc.tik-choco.com/signaling");
    let nostr = config
        .signaling
        .nostr
        .as_ref()
        .expect("default Nostr config should be present");
    assert!(nostr.relays.is_empty());
}

#[test]
fn full_config_without_signaling_uses_default_nostr_mode() {
    let json = r#"{
        "signalingUrl": "wss://example.invalid/signaling",
        "limits": {
            "max_connection_count": 30,
            "expire_node_seconds": 10.0,
            "hop_count": 2,
            "reserved_connection_count": 1,
            "force_disconnect_count": 0
        },
        "dnve": {
            "density_max_range": 64.0,
            "distance_layers": 1,
            "density_resolution": 6,
            "density_encoding": "byte",
            "spatial_partition_type": "dodecahedron",
            "direction_threshold": 0.0,
            "aoi_range": 10.0,
            "connection_mode": "node_list_aoi_guard",
            "node_list_exchange_mode": "pull"
        },
        "intervals": {
            "connection_balancer": 2.0,
            "heartbeat": 1.0,
            "node_list": 2.0
        },
        "webrtc": {
            "iceServers": []
        }
    }"#;

    let config: Config = serde_json::from_str(json).expect("old full config should deserialize");
    assert_eq!(config.signaling.mode, SignalingMode::Nostr);
    assert!(config.signaling.nostr.is_some());
}

#[test]
fn nostr_signaling_config_parses_from_flat_update() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(
        r#"{
            "signaling": {
                "mode": "nostr",
                "nostr": {
                    "relays": ["ws://127.0.0.1:7777"],
                    "discoveryKind": 25049,
                    "messageKind": 25050,
                    "ttlSeconds": 10,
                    "inviteSalt": "nostr-sig-test-local-salt",
                    "inviteCode": "dev-invite-001"
                }
            }
        }"#,
    );

    assert!(updated.is_ok(), "Nostr flat config should parse");
    assert_eq!(config.signaling.mode, SignalingMode::Nostr);
    let nostr = config
        .signaling
        .nostr
        .as_ref()
        .expect("Nostr config should be present");
    assert_eq!(nostr.relays, vec!["ws://127.0.0.1:7777"]);
    assert_eq!(nostr.discovery_kind, 25049);
    assert_eq!(nostr.message_kind, 25050);
    assert_eq!(nostr.ttl_seconds, 10);
}

#[test]
fn empty_relays_with_default_invite_is_accepted_for_implicit_relay_list() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(
        r#"{
            "signaling": {
                "mode": "nostr",
                "nostr": {
                    "relays": [],
                    "discoveryKind": 25049,
                    "messageKind": 25050,
                    "ttlSeconds": 10,
                    "inviteSalt": "nostr-sig-test-local-salt",
                    "inviteCode": "dev-invite-001"
                }
            }
        }"#,
    );

    assert!(
        updated.is_ok(),
        "empty relays (implicit relay list URL) should be accepted with default invite"
    );
    assert_eq!(config.signaling.mode, SignalingMode::Nostr);
    let nostr = config
        .signaling
        .nostr
        .as_ref()
        .expect("Nostr config should be present");
    assert!(nostr.relays.is_empty());
}

#[test]
fn empty_relays_use_default_relay_list_url_with_custom_invite() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(
        r#"{
            "signaling": {
                "mode": "nostr",
                "nostr": {
                    "relays": [],
                    "discoveryKind": 25049,
                    "messageKind": 25050,
                    "ttlSeconds": 10,
                    "inviteSalt": "app-specific-random-salt",
                    "inviteCode": "invite-code-with-real-entropy"
                }
            }
        }"#,
    );

    assert!(
        updated.is_ok(),
        "empty relays should use the default relay list URL when invite is custom"
    );
    let nostr = config
        .signaling
        .nostr
        .as_ref()
        .expect("Nostr config should be present");
    assert!(nostr.relays.is_empty());
    assert_eq!(nostr.relay_list_url, None);
    assert_eq!(
        nostr.effective_relay_list_url(),
        Some(DEFAULT_RELAY_LIST_URL)
    );
}

#[test]
fn default_nostr_invite_is_rejected_for_public_relays() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(
        r#"{
            "signaling": {
                "mode": "nostr",
                "nostr": {
                    "relays": ["wss://relay.example.invalid"],
                    "discoveryKind": 25049,
                    "messageKind": 25050,
                    "ttlSeconds": 10,
                    "inviteSalt": "nostr-sig-test-local-salt",
                    "inviteCode": "dev-invite-001"
                }
            }
        }"#,
    );

    assert!(
        updated.is_err(),
        "public relays require a non-default invite"
    );
    let nostr = config
        .signaling
        .nostr
        .as_ref()
        .expect("default Nostr config should remain present");
    assert!(nostr.relays.is_empty());
}

#[test]
fn custom_nostr_invite_is_allowed_for_public_relays() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(
        r#"{
            "signaling": {
                "mode": "nostr",
                "nostr": {
                    "relays": ["wss://relay.example.invalid"],
                    "discoveryKind": 25049,
                    "messageKind": 25050,
                    "ttlSeconds": 10,
                    "inviteSalt": "app-specific-random-salt",
                    "inviteCode": "invite-code-with-real-entropy"
                }
            }
        }"#,
    );

    assert!(
        updated.is_ok(),
        "custom invite should be accepted for public relays"
    );
    let nostr = config
        .signaling
        .nostr
        .as_ref()
        .expect("Nostr config should be present");
    assert_eq!(nostr.relays, vec!["wss://relay.example.invalid"]);
}

#[test]
fn nostr_relay_list_url_parses_from_config() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(&format!(
        r#"{{
            "signaling": {{
                "mode": "nostr",
                "nostr": {{
                    "relayListUrl": "{DEFAULT_RELAY_LIST_URL}",
                    "discoveryKind": 25049,
                    "messageKind": 25050,
                    "ttlSeconds": 10,
                    "inviteSalt": "app-specific-random-salt",
                    "inviteCode": "invite-code-with-real-entropy"
                }}
            }}
        }}"#
    ));

    assert!(
        updated.is_ok(),
        "relayListUrl should allow relays to be loaded later"
    );
    let nostr = config
        .signaling
        .nostr
        .as_ref()
        .expect("Nostr config should be present");
    assert!(nostr.relays.is_empty());
    assert_eq!(
        nostr.relay_list_url.as_deref(),
        Some(DEFAULT_RELAY_LIST_URL)
    );
}

#[test]
fn default_invite_is_rejected_for_public_relay_list_url() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(&format!(
        r#"{{
            "signaling": {{
                "mode": "nostr",
                "nostr": {{
                    "relays": [],
                    "relayListUrl": "{DEFAULT_RELAY_LIST_URL}",
                    "discoveryKind": 25049,
                    "messageKind": 25050,
                    "ttlSeconds": 10,
                    "inviteSalt": "nostr-sig-test-local-salt",
                    "inviteCode": "dev-invite-001"
                }}
            }}
        }}"#
    ));

    assert!(
        updated.is_err(),
        "public relayListUrl requires a non-default invite"
    );
}
