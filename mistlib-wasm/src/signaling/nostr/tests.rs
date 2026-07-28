use super::connection::is_relay_idle;
use super::refresh::jittered_discovery_refresh_delay_ms;
use super::WasmNostrSignaler;
use futures::future::{select, Either};
use gloo_timers::future::TimeoutFuture;
use js_sys::Reflect;
use mistlib_core::config::NostrSignalingConfig;
use mistlib_core::signaling::nostr::{
    build_message_event, build_message_event_with_sequence,
    build_message_event_with_sequence_and_joined_at, message_filter, random_subscription_id,
    NostrCodecConfig, NostrCrypto, TAG_INVITE_SCOPE,
};
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::types::NodeId;
use tokio::sync::mpsc;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;
use web_time::Duration;

#[wasm_bindgen_test]
fn nostr_message_subscription_uses_room_scope() {
    let config = NostrSignalingConfig::default();
    let codec_config = NostrCodecConfig::from_config(&config);
    let signaler = WasmNostrSignaler::new(NodeId("wasm-local".to_string()), config);
    let room_id = "wasm-nostr-test-room";

    let [_discovery_frame, message_frame] = signaler.subscription_frames(room_id).unwrap();
    let actual_scopes = subscription_scope_values(&message_frame);
    let expected_filter = message_filter(
        &codec_config,
        room_id,
        &signaler.current_identity().public_key,
    );
    let tag_name = format!("#{TAG_INVITE_SCOPE}");
    let expected_scopes = expected_filter.tag_filters.get(&tag_name).unwrap();

    assert_eq!(actual_scopes, *expected_scopes);
    assert!(
        !actual_scopes.contains(&signaler.identity.public_key),
        "message subscription must use room scope, not the local Nostr pubkey"
    );
}

fn subscription_scope_values(frame: &str) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_str(frame).unwrap();
    let items = value.as_array().unwrap();
    assert_eq!(items.first().unwrap().as_str(), Some("REQ"));
    let filter = items.get(2).unwrap();
    let tag_name = format!("#{TAG_INVITE_SCOPE}");
    filter
        .get(&tag_name)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect()
}

#[wasm_bindgen_test]
fn nostr_subscription_frames_reuse_stable_subscription_ids_across_calls() {
    // The periodic keepalive re-subscribe (see keepalive.rs) relies on
    // resending REQ with the SAME subscription id so relays treat it as a
    // NIP-01 filter replace instead of piling up a new subscription every
    // cycle. `subscription_frames` must therefore return stable ids across
    // repeated calls on the same instance.
    let config = NostrSignalingConfig::default();
    let signaler = WasmNostrSignaler::new(NodeId("wasm-local".to_string()), config);

    let [discovery_1, message_1] = signaler.subscription_frames("room-a").unwrap();
    let [discovery_2, message_2] = signaler.subscription_frames("room-a").unwrap();

    assert_eq!(
        subscription_id(&discovery_1),
        subscription_id(&discovery_2),
        "discovery subscription id must stay stable across re-subscribes"
    );
    assert_eq!(
        subscription_id(&message_1),
        subscription_id(&message_2),
        "message subscription id must stay stable across re-subscribes"
    );
    assert_ne!(
        subscription_id(&discovery_1),
        subscription_id(&message_1),
        "discovery and message subscriptions must use distinct ids"
    );
}

fn subscription_id(frame: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(frame).unwrap();
    value.as_array().unwrap()[1].as_str().unwrap().to_string()
}

#[wasm_bindgen_test]
fn nostr_relay_idle_threshold_is_two_keepalive_cycles() {
    // Documents/enforces the "60s = 2 keepalive cycles" design ratio
    // regardless of the prod (30s/60s) vs test (200ms/400ms) constant
    // values in mistlib-wasm/src/signaling/nostr.rs.
    assert_eq!(
        u64::from(super::RELAY_IDLE_THRESHOLD_MS),
        u64::from(super::RELAY_KEEPALIVE_INTERVAL_MS) * 2
    );
}

#[wasm_bindgen_test]
fn nostr_relay_idle_predicate_matches_threshold_boundary() {
    assert!(!is_relay_idle(Duration::from_millis(399), 400));
    assert!(is_relay_idle(Duration::from_millis(400), 400));
    assert!(is_relay_idle(Duration::from_secs(61), 60_000));
}

#[wasm_bindgen_test]
fn nostr_discovery_refresh_jitter_stays_within_quarter_of_base() {
    // ttl_seconds=60 -> base = 60 * 500ms = 30_000ms; jitter is +/-25%.
    assert_eq!(jittered_discovery_refresh_delay_ms(60, 0.0), 30_000);
    assert_eq!(jittered_discovery_refresh_delay_ms(60, -1.0), 22_500); // clamped to -0.25
    assert_eq!(jittered_discovery_refresh_delay_ms(60, 1.0), 37_500); // clamped to +0.25
}

#[wasm_bindgen_test]
fn nostr_message_processing_touches_discovery_node_with_local_ttl_not_sender_ttl() {
    // handler.rs calls `table.touch_node(&incoming.sender_id,
    // self.codec_config.ttl_seconds)` right after binding, using the
    // RECEIVER's own ttl_seconds rather than the sender's declared
    // `decoded.expires_at`. Give alice a much longer ttl than bob so the
    // resulting discovery-table expiry can only be explained by touch_node
    // having run with alice's local ttl. Both ttl_seconds are kept at or
    // above `NostrCodecConfig::room_scope_rotation_seconds`'s 3600s clamp
    // ceiling so alice and bob still land on the same room-scope rotation
    // bucket (room scope only tracks the *clamped* rotation period, not the
    // raw ttl_seconds used for expires_at/touch_node).
    let alice_config = NostrSignalingConfig {
        ttl_seconds: 7_200,
        ..NostrSignalingConfig::default()
    };
    let bob_config = NostrSignalingConfig {
        ttl_seconds: 3_600,
        ..alice_config.clone()
    };
    let alice_id = NodeId("wasm-alice".to_string());
    let bob_id = NodeId("wasm-bob".to_string());
    let alice = WasmNostrSignaler::new(alice_id, alice_config);
    let bob = WasmNostrSignaler::new(bob_id.clone(), bob_config);
    let (tx, _rx) = mpsc::unbounded_channel();
    alice.set_room_id("room-a").unwrap();
    alice
        .requested_pubkeys
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(bob.identity.public_key.clone());

    let event = build_message_event_with_sequence(
        &bob.codec_config,
        &bob.crypto,
        &bob.identity,
        &alice.identity.public_key,
        &SignalingData {
            sender_id: bob_id.clone(),
            receiver_id: NodeId::broadcast(),
            room_id: "room-a".to_string(),
            data: String::new(),
            signaling_type: SignalingType::Request,
        },
        1,
    )
    .unwrap();

    alice.process_event(event, &tx).unwrap();

    let expires_at = alice
        .discovery_table
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expires_at_for_pubkey(&bob.identity.public_key)
        .expect("bob should be bound in alice's discovery table");
    assert!(
        expires_at >= now_unix_seconds() + 7_000,
        "touch_node should extend the entry using alice's own ttl_seconds (7200s), \
         not bob's declared ttl (3600s); got expires_at={expires_at}"
    );
}

fn now_unix_seconds() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[wasm_bindgen_test]
fn nostr_non_recipient_room_mailbox_message_is_ignored() {
    let config = NostrSignalingConfig::default();
    let alice = WasmNostrSignaler::new(NodeId("wasm-alice".to_string()), config.clone());
    let bob = WasmNostrSignaler::new(NodeId("wasm-bob".to_string()), config.clone());
    let charlie = WasmNostrSignaler::new(NodeId("wasm-charlie".to_string()), config);
    let (tx, mut rx) = mpsc::unbounded_channel();
    charlie.set_room_id("room-a").unwrap();
    let event = build_message_event(
        &alice.codec_config,
        &alice.crypto,
        &alice.identity,
        &bob.identity.public_key,
        &SignalingData {
            sender_id: NodeId("wasm-alice".to_string()),
            receiver_id: NodeId("wasm-bob".to_string()),
            room_id: "room-a".to_string(),
            data: "v=0\r\nsdp".to_string(),
            signaling_type: SignalingType::Offer,
        },
    )
    .unwrap();

    charlie.process_event(event, &tx).unwrap();

    assert!(rx.try_recv().is_err());
}

#[wasm_bindgen_test]
fn nostr_unrequested_non_request_payload_is_dropped() {
    let config = NostrSignalingConfig::default();
    let alice = WasmNostrSignaler::new(NodeId("wasm-alice".to_string()), config.clone());
    let mallory = WasmNostrSignaler::new(NodeId("wasm-mallory".to_string()), config);
    let (tx, mut rx) = mpsc::unbounded_channel();
    alice.set_room_id("room-a").unwrap();
    let event = build_message_event_with_sequence(
        &mallory.codec_config,
        &mallory.crypto,
        &mallory.identity,
        &alice.identity.public_key,
        &SignalingData {
            sender_id: NodeId("wasm-mallory".to_string()),
            receiver_id: NodeId("wasm-alice".to_string()),
            room_id: "room-a".to_string(),
            data: "candidate".to_string(),
            signaling_type: SignalingType::Candidate,
        },
        1,
    )
    .unwrap();

    alice.process_event(event, &tx).unwrap();

    assert!(rx.try_recv().is_err());
}

#[wasm_bindgen_test]
fn nostr_replayed_payload_with_new_event_id_is_dropped() {
    let config = NostrSignalingConfig::default();
    let alice = WasmNostrSignaler::new(NodeId("wasm-alice".to_string()), config.clone());
    let bob = WasmNostrSignaler::new(NodeId("wasm-bob".to_string()), config);
    let (tx, mut rx) = mpsc::unbounded_channel();
    alice.set_room_id("room-a").unwrap();
    alice
        .requested_pubkeys
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(bob.identity.public_key.clone());
    let payload = SignalingData {
        sender_id: NodeId("wasm-bob".to_string()),
        receiver_id: NodeId("wasm-alice".to_string()),
        room_id: "room-a".to_string(),
        data: "offer".to_string(),
        signaling_type: SignalingType::Offer,
    };
    let event = build_message_event_with_sequence(
        &bob.codec_config,
        &bob.crypto,
        &bob.identity,
        &alice.identity.public_key,
        &payload,
        1,
    )
    .unwrap();
    let mut replay = event.clone();
    replay.created_at = replay.created_at.saturating_add(1);
    replay.refresh_id();
    replay.sig = bob.crypto.sign_event(&bob.identity, &replay).unwrap();

    alice.process_event(event, &tx).unwrap();
    match rx.try_recv().unwrap() {
        MessageContent::Data(received) => assert_eq!(received, payload),
        other => panic!("unexpected signaling message: {other:?}"),
    }

    alice.process_event(replay, &tx).unwrap();
    assert!(rx.try_recv().is_err());
}

#[wasm_bindgen_test]
fn nostr_requested_rejoin_request_rebinds_same_node_id_to_new_pubkey() {
    let config = NostrSignalingConfig::default();
    let alice_id = NodeId("wasm-alice".to_string());
    let bob_id = NodeId("wasm-bob".to_string());
    let alice = WasmNostrSignaler::new(alice_id, config.clone());
    let old_bob = WasmNostrSignaler::new(bob_id.clone(), config.clone());
    let new_bob = WasmNostrSignaler::new(bob_id.clone(), config);
    let (tx, _rx) = mpsc::unbounded_channel();
    alice.set_room_id("room-a").unwrap();

    alice
        .requested_pubkeys
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(old_bob.identity.public_key.clone());
    let first_request = build_message_event_with_sequence(
        &old_bob.codec_config,
        &old_bob.crypto,
        &old_bob.identity,
        &alice.identity.public_key,
        &SignalingData {
            sender_id: bob_id.clone(),
            receiver_id: NodeId::broadcast(),
            room_id: "room-a".to_string(),
            data: String::new(),
            signaling_type: SignalingType::Request,
        },
        1,
    )
    .unwrap();
    alice.process_event(first_request, &tx).unwrap();
    assert_eq!(
        alice
            .discovery_table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pubkey_for_node(&bob_id),
        Some(old_bob.identity.public_key.clone())
    );

    alice
        .requested_pubkeys
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(new_bob.identity.public_key.clone());
    let rejoin_request = build_message_event_with_sequence(
        &new_bob.codec_config,
        &new_bob.crypto,
        &new_bob.identity,
        &alice.identity.public_key,
        &SignalingData {
            sender_id: bob_id.clone(),
            receiver_id: NodeId::broadcast(),
            room_id: "room-a".to_string(),
            data: String::new(),
            signaling_type: SignalingType::Request,
        },
        1,
    )
    .unwrap();
    alice.process_event(rejoin_request, &tx).unwrap();

    let mut table = alice
        .discovery_table
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        table.pubkey_for_node(&bob_id),
        Some(new_bob.identity.public_key.clone())
    );
    assert_eq!(
        table.expires_at_for_pubkey(&old_bob.identity.public_key),
        None
    );
}

#[wasm_bindgen_test]
fn nostr_rebind_via_newer_epoch_accepts_offer_and_emits_rejoin_before_forwarding_it() {
    // The core repro: a browser peer (bob) reloads the page, regenerating
    // its temporary Nostr signaling keypair while keeping the same
    // host-supplied NodeId. Its first message to alice under the new pubkey
    // is an Offer (never a `Request`), addressed directly at alice (not a
    // broadcast), and alice has never `Request`-ed the new pubkey. Both the
    // sender-acceptance gate (Task 5) and the rebind itself must key off the
    // peer-declared epoch to let this through, and the transport must be
    // told to tear down the stale connection via a `Rejoin` BEFORE the Offer
    // is forwarded.
    let config = NostrSignalingConfig::default();
    let alice_id = NodeId("wasm-alice".to_string());
    let bob_id = NodeId("wasm-bob".to_string());
    let alice = WasmNostrSignaler::new(alice_id.clone(), config.clone());
    let old_bob = WasmNostrSignaler::new(bob_id.clone(), config.clone());
    let new_bob = WasmNostrSignaler::new(bob_id.clone(), config);
    let (tx, mut rx) = mpsc::unbounded_channel();
    alice.set_room_id("room-a").unwrap();

    let old_epoch = 1_000u64;
    alice
        .requested_pubkeys
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(old_bob.identity.public_key.clone());
    let bind_event = build_message_event_with_sequence_and_joined_at(
        &old_bob.codec_config,
        &old_bob.crypto,
        &old_bob.identity,
        &alice.identity.public_key,
        &SignalingData {
            sender_id: bob_id.clone(),
            receiver_id: NodeId::broadcast(),
            room_id: "room-a".to_string(),
            data: String::new(),
            signaling_type: SignalingType::Request,
        },
        1,
        Some(old_epoch),
    )
    .unwrap();
    alice.process_event(bind_event, &tx).unwrap();
    while rx.try_recv().is_ok() {}
    assert_eq!(
        alice
            .discovery_table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pubkey_for_node(&bob_id),
        Some(old_bob.identity.public_key.clone())
    );

    let new_epoch = old_epoch + 1;
    let offer_payload = SignalingData {
        sender_id: bob_id.clone(),
        receiver_id: alice_id.clone(),
        room_id: "room-a".to_string(),
        data: "v=0\r\ns=new-session".to_string(),
        signaling_type: SignalingType::Offer,
    };
    let offer_event = build_message_event_with_sequence_and_joined_at(
        &new_bob.codec_config,
        &new_bob.crypto,
        &new_bob.identity,
        &alice.identity.public_key,
        &offer_payload,
        1,
        Some(new_epoch),
    )
    .unwrap();

    alice.process_event(offer_event, &tx).unwrap();

    match rx.try_recv().unwrap() {
        MessageContent::Data(data) => {
            assert_eq!(data.signaling_type, SignalingType::Rejoin);
            assert_eq!(data.sender_id, bob_id);
            assert_eq!(data.receiver_id, alice_id);
            assert_eq!(data.room_id, "room-a");
            assert_eq!(data.data, new_epoch.to_string());
        }
        other => panic!("expected a Rejoin notification first, got {other:?}"),
    }
    match rx.try_recv().unwrap() {
        MessageContent::Data(data) => assert_eq!(data, offer_payload),
        other => panic!("expected the triggering Offer next, got {other:?}"),
    }
    assert!(rx.try_recv().is_err());

    let mut table = alice
        .discovery_table
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        table.pubkey_for_node(&bob_id),
        Some(new_bob.identity.public_key.clone())
    );
    assert_eq!(
        table.expires_at_for_pubkey(&old_bob.identity.public_key),
        None
    );
}

#[wasm_bindgen_test]
fn nostr_equal_epoch_offer_from_new_pubkey_is_rejected_not_treated_as_rebind() {
    // Epochs must be STRICTLY newer to win a rebind (mirrors
    // `DiscoveryTable::bind_node_with_epoch`'s own guard): a replayed or
    // merely-equal `sender_joined_at` must not let a stranger under a fresh
    // pubkey steal an existing node id's binding.
    let config = NostrSignalingConfig::default();
    let alice_id = NodeId("wasm-alice".to_string());
    let bob_id = NodeId("wasm-bob".to_string());
    let alice = WasmNostrSignaler::new(alice_id.clone(), config.clone());
    let old_bob = WasmNostrSignaler::new(bob_id.clone(), config.clone());
    let new_bob = WasmNostrSignaler::new(bob_id.clone(), config);
    let (tx, mut rx) = mpsc::unbounded_channel();
    alice.set_room_id("room-a").unwrap();

    let epoch = 1_000u64;
    alice
        .requested_pubkeys
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(old_bob.identity.public_key.clone());
    let bind_event = build_message_event_with_sequence_and_joined_at(
        &old_bob.codec_config,
        &old_bob.crypto,
        &old_bob.identity,
        &alice.identity.public_key,
        &SignalingData {
            sender_id: bob_id.clone(),
            receiver_id: NodeId::broadcast(),
            room_id: "room-a".to_string(),
            data: String::new(),
            signaling_type: SignalingType::Request,
        },
        1,
        Some(epoch),
    )
    .unwrap();
    alice.process_event(bind_event, &tx).unwrap();
    while rx.try_recv().is_ok() {}

    let offer_event = build_message_event_with_sequence_and_joined_at(
        &new_bob.codec_config,
        &new_bob.crypto,
        &new_bob.identity,
        &alice.identity.public_key,
        &SignalingData {
            sender_id: bob_id.clone(),
            receiver_id: alice_id.clone(),
            room_id: "room-a".to_string(),
            data: "v=0\r\ns=impostor".to_string(),
            signaling_type: SignalingType::Offer,
        },
        1,
        Some(epoch), // Equal to the stored epoch: not strictly newer.
    )
    .unwrap();

    alice.process_event(offer_event, &tx).unwrap();

    assert!(
        rx.try_recv().is_err(),
        "an equal (non-newer) epoch must not be treated as a legitimate rebind"
    );
    assert_eq!(
        alice
            .discovery_table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pubkey_for_node(&bob_id),
        Some(old_bob.identity.public_key.clone()),
        "the node id must remain bound to the original pubkey"
    );
}

#[wasm_bindgen_test]
fn nostr_rebind_purges_per_peer_state_keyed_by_the_dead_pubkey() {
    let config = NostrSignalingConfig::default();
    let alice_id = NodeId("wasm-alice".to_string());
    let bob_id = NodeId("wasm-bob".to_string());
    let alice = WasmNostrSignaler::new(alice_id.clone(), config.clone());
    let old_bob = WasmNostrSignaler::new(bob_id.clone(), config.clone());
    let new_bob = WasmNostrSignaler::new(bob_id.clone(), config);
    let (tx, mut rx) = mpsc::unbounded_channel();
    alice.set_room_id("room-a").unwrap();
    let old_pubkey = old_bob.identity.public_key.clone();

    alice
        .requested_pubkeys
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(old_pubkey.clone());
    let bind_event = build_message_event_with_sequence_and_joined_at(
        &old_bob.codec_config,
        &old_bob.crypto,
        &old_bob.identity,
        &alice.identity.public_key,
        &SignalingData {
            sender_id: bob_id.clone(),
            receiver_id: NodeId::broadcast(),
            room_id: "room-a".to_string(),
            data: String::new(),
            signaling_type: SignalingType::Request,
        },
        1,
        Some(1_000),
    )
    .unwrap();
    alice.process_event(bind_event, &tx).unwrap();
    while rx.try_recv().is_ok() {}

    // Simulate additional per-peer bookkeeping accumulated against the old
    // pubkey during the session, beyond what the initial bind itself set.
    alice
        .incoming_sequences
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(old_pubkey.clone(), 5);
    alice
        .outgoing_sequences
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(old_pubkey.clone(), 7);
    alice
        .peer_sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(old_pubkey.clone(), 1_000);
    assert!(alice
        .requested_pubkeys
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&old_pubkey));

    let offer_event = build_message_event_with_sequence_and_joined_at(
        &new_bob.codec_config,
        &new_bob.crypto,
        &new_bob.identity,
        &alice.identity.public_key,
        &SignalingData {
            sender_id: bob_id.clone(),
            receiver_id: alice_id.clone(),
            room_id: "room-a".to_string(),
            data: "v=0\r\ns=new-session".to_string(),
            signaling_type: SignalingType::Offer,
        },
        1,
        Some(1_001),
    )
    .unwrap();
    alice.process_event(offer_event, &tx).unwrap();

    assert!(!alice
        .requested_pubkeys
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&old_pubkey));
    assert!(!alice
        .incoming_sequences
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&old_pubkey));
    assert!(!alice
        .outgoing_sequences
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&old_pubkey));
    assert!(!alice
        .peer_sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&old_pubkey));
}

#[wasm_bindgen_test]
fn nostr_rejoin_signal_arriving_from_the_wire_is_rejected() {
    // `Rejoin` is locally-synthesized only (see `SignalingType::is_local_only`
    // and Task 4's outbound guard); nothing in this codebase ever asks the
    // codec to publish one. But a non-conforming or malicious peer could
    // still hand-craft the wire bytes directly, attempting to make alice
    // tear down a live peer connection out from under it. It must be
    // dropped, not forwarded.
    let config = NostrSignalingConfig::default();
    let alice = WasmNostrSignaler::new(NodeId("wasm-alice".to_string()), config.clone());
    let mallory = WasmNostrSignaler::new(NodeId("wasm-mallory".to_string()), config);
    let (tx, mut rx) = mpsc::unbounded_channel();
    alice.set_room_id("room-a").unwrap();

    let forged_rejoin = build_message_event_with_sequence_and_joined_at(
        &mallory.codec_config,
        &mallory.crypto,
        &mallory.identity,
        &alice.identity.public_key,
        &SignalingData {
            sender_id: NodeId("wasm-bob".to_string()),
            receiver_id: NodeId("wasm-alice".to_string()),
            room_id: "room-a".to_string(),
            data: "999999".to_string(),
            signaling_type: SignalingType::Rejoin,
        },
        1,
        Some(999_999),
    )
    .unwrap();

    alice.process_event(forged_rejoin, &tx).unwrap();

    assert!(rx.try_recv().is_err());
    assert_eq!(
        alice
            .discovery_table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pubkey_for_node(&NodeId("wasm-bob".to_string())),
        None,
        "a wire-delivered Rejoin must not create/alter any discovery binding"
    );
}

#[wasm_bindgen_test]
fn nostr_known_peer_unchanged_pubkey_message_forwards_without_rejoin() {
    let config = NostrSignalingConfig::default();
    let alice_id = NodeId("wasm-alice".to_string());
    let bob_id = NodeId("wasm-bob".to_string());
    let alice = WasmNostrSignaler::new(alice_id.clone(), config.clone());
    let bob = WasmNostrSignaler::new(bob_id.clone(), config);
    let (tx, mut rx) = mpsc::unbounded_channel();
    alice.set_room_id("room-a").unwrap();
    alice
        .requested_pubkeys
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(bob.identity.public_key.clone());

    let bind_event = build_message_event_with_sequence_and_joined_at(
        &bob.codec_config,
        &bob.crypto,
        &bob.identity,
        &alice.identity.public_key,
        &SignalingData {
            sender_id: bob_id.clone(),
            receiver_id: NodeId::broadcast(),
            room_id: "room-a".to_string(),
            data: String::new(),
            signaling_type: SignalingType::Request,
        },
        1,
        Some(1_000),
    )
    .unwrap();
    alice.process_event(bind_event, &tx).unwrap();
    while rx.try_recv().is_ok() {}
    assert_eq!(
        alice
            .discovery_table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pubkey_for_node(&bob_id),
        Some(bob.identity.public_key.clone())
    );

    let payload = SignalingData {
        sender_id: bob_id.clone(),
        receiver_id: alice_id.clone(),
        room_id: "room-a".to_string(),
        data: "candidate".to_string(),
        signaling_type: SignalingType::Candidate,
    };
    let event = build_message_event_with_sequence_and_joined_at(
        &bob.codec_config,
        &bob.crypto,
        &bob.identity,
        &alice.identity.public_key,
        &payload,
        2,
        Some(1_000),
    )
    .unwrap();

    alice.process_event(event, &tx).unwrap();

    match rx.try_recv().unwrap() {
        MessageContent::Data(data) => assert_eq!(data, payload),
        other => panic!("unexpected signaling message: {other:?}"),
    }
    assert!(
        rx.try_recv().is_err(),
        "an unchanged pubkey must not produce a Rejoin notification"
    );
}

#[wasm_bindgen_test(async)]
async fn nostr_live_relay_exchanges_request_when_configured() {
    if env_string("MIST_NOSTR_SIX_NODES").is_some()
        || env_string("MIST_NOSTR_MIXED_SIX_NODES").is_some()
    {
        return;
    }
    let Some(relay_url) = env_string("MIST_NOSTR_RELAY_URL") else {
        return;
    };

    let room_id = "wasm-nostr-live-room";
    let alice_id = NodeId("wasm-alice".to_string());
    let bob_id = NodeId("wasm-bob".to_string());
    let alice = WasmNostrSignaler::new(alice_id.clone(), live_config(&relay_url));
    let bob = WasmNostrSignaler::new(bob_id.clone(), live_config(&relay_url));
    let (alice_tx, mut alice_rx) = mpsc::unbounded_channel();
    let (bob_tx, mut bob_rx) = mpsc::unbounded_channel();

    alice.connect(alice_tx).await.unwrap();
    bob.connect(bob_tx).await.unwrap();

    alice
        .send_signaling(
            &NodeId::broadcast(),
            MessageContent::Data(discovery_signal(&alice_id, room_id)),
        )
        .await
        .unwrap();
    bob.send_signaling(
        &NodeId::broadcast(),
        MessageContent::Data(discovery_signal(&bob_id, room_id)),
    )
    .await
    .unwrap();

    let received = recv_signaling_data(&mut bob_rx).await;
    assert_eq!(received.room_id, room_id);
    assert_eq!(received.sender_id, alice_id);
    assert_eq!(received.signaling_type, SignalingType::Request);

    alice.close().await.unwrap();
    bob.close().await.unwrap();

    while alice_rx.try_recv().is_ok() {}
}

#[wasm_bindgen_test(async)]
async fn nostr_live_relay_reconnects_rejoined_node_with_same_id_when_configured() {
    if env_string("MIST_NOSTR_SIX_NODES").is_some()
        || env_string("MIST_NOSTR_MIXED_SIX_NODES").is_some()
    {
        return;
    }
    let Some(relay_url) = env_string("MIST_NOSTR_RELAY_URL") else {
        return;
    };

    let room_id = format!("wasm-nostr-rejoin-room-{}", random_subscription_id());
    let alice_id = NodeId(format!("wasm-alice-{room_id}"));
    let bob_id = NodeId(format!("wasm-bob-{room_id}"));
    let alice = WasmNostrSignaler::new(alice_id.clone(), live_config(&relay_url));
    let bob = WasmNostrSignaler::new(bob_id.clone(), live_config(&relay_url));
    let (alice_tx, mut alice_rx) = mpsc::unbounded_channel();
    let (bob_tx, mut bob_rx) = mpsc::unbounded_channel();

    alice.connect(alice_tx).await.unwrap();
    bob.connect(bob_tx).await.unwrap();

    alice
        .send_signaling(
            &NodeId::broadcast(),
            MessageContent::Data(discovery_signal(&alice_id, &room_id)),
        )
        .await
        .unwrap();
    bob.send_signaling(
        &NodeId::broadcast(),
        MessageContent::Data(discovery_signal(&bob_id, &room_id)),
    )
    .await
    .unwrap();

    let initial_bob_pubkey = bob.current_identity().public_key;
    wait_discovery_binding(&alice, &bob_id, &initial_bob_pubkey).await;

    alice
        .send_signaling(
            &bob_id,
            MessageContent::Data(offer_signal(&alice_id, &bob_id, &room_id)),
        )
        .await
        .unwrap();
    let initial_offer = recv_matching_signaling_data(&mut bob_rx, |data| {
        data.sender_id == alice_id
            && data.receiver_id == bob_id
            && data.signaling_type == SignalingType::Offer
    })
    .await;
    assert_eq!(initial_offer.sender_id, alice_id);
    assert_eq!(initial_offer.receiver_id, bob_id);
    assert_eq!(initial_offer.signaling_type, SignalingType::Offer);

    bob.close().await.unwrap();
    drain_receiver(&mut alice_rx).await;
    drain_receiver(&mut bob_rx).await;

    let rejoined_bob = WasmNostrSignaler::new(bob_id.clone(), live_config(&relay_url));
    let (rejoined_bob_tx, mut rejoined_bob_rx) = mpsc::unbounded_channel();
    rejoined_bob.connect(rejoined_bob_tx).await.unwrap();
    rejoined_bob
        .send_signaling(
            &NodeId::broadcast(),
            MessageContent::Data(discovery_signal(&bob_id, &room_id)),
        )
        .await
        .unwrap();

    let rejoined_bob_pubkey = rejoined_bob.current_identity().public_key;
    assert_ne!(initial_bob_pubkey, rejoined_bob_pubkey);
    wait_discovery_binding(&alice, &bob_id, &rejoined_bob_pubkey).await;

    alice
        .send_signaling(
            &bob_id,
            MessageContent::Data(offer_signal(&alice_id, &bob_id, &room_id)),
        )
        .await
        .unwrap();
    let rejoin_offer = recv_matching_signaling_data(&mut rejoined_bob_rx, |data| {
        data.sender_id == alice_id
            && data.receiver_id == bob_id
            && data.signaling_type == SignalingType::Offer
    })
    .await;
    assert_eq!(rejoin_offer.sender_id, alice_id);
    assert_eq!(rejoin_offer.receiver_id, bob_id);
    assert_eq!(rejoin_offer.signaling_type, SignalingType::Offer);

    alice.close().await.unwrap();
    rejoined_bob.close().await.unwrap();
}

#[wasm_bindgen_test(async)]
async fn nostr_live_relay_reconnects_rejoined_node_with_same_id_among_four_nodes_when_configured() {
    if env_string("MIST_NOSTR_SIX_NODES").is_some()
        || env_string("MIST_NOSTR_MIXED_SIX_NODES").is_some()
    {
        return;
    }
    let Some(relay_url) = env_string("MIST_NOSTR_RELAY_URL") else {
        return;
    };

    let run_id = random_subscription_id();
    let room_id = format!("wasm-nostr-four-rejoin-room-{run_id}");
    let alice_id = NodeId(format!("wasm-alice-{run_id}"));
    let bob_id = NodeId(format!("wasm-bob-{run_id}"));
    let carol_id = NodeId(format!("wasm-carol-{run_id}"));
    let dave_id = NodeId(format!("wasm-dave-{run_id}"));

    let alice = WasmNostrSignaler::new(alice_id.clone(), live_config(&relay_url));
    let bob = WasmNostrSignaler::new(bob_id.clone(), live_config(&relay_url));
    let carol = WasmNostrSignaler::new(carol_id.clone(), live_config(&relay_url));
    let dave = WasmNostrSignaler::new(dave_id.clone(), live_config(&relay_url));
    let (alice_tx, mut alice_rx) = mpsc::unbounded_channel();
    let (bob_tx, mut bob_rx) = mpsc::unbounded_channel();
    let (carol_tx, mut carol_rx) = mpsc::unbounded_channel();
    let (dave_tx, mut dave_rx) = mpsc::unbounded_channel();

    alice.connect(alice_tx).await.unwrap();
    bob.connect(bob_tx).await.unwrap();
    carol.connect(carol_tx).await.unwrap();
    dave.connect(dave_tx).await.unwrap();

    let stable_nodes = [(&alice_id, &alice), (&carol_id, &carol), (&dave_id, &dave)];

    for (id, signaler) in [
        (&alice_id, &alice),
        (&bob_id, &bob),
        (&carol_id, &carol),
        (&dave_id, &dave),
    ] {
        signaler
            .send_signaling(
                &NodeId::broadcast(),
                MessageContent::Data(discovery_signal(id, &room_id)),
            )
            .await
            .unwrap();
        TimeoutFuture::new(100).await;
    }

    let initial_bob_pubkey = bob.current_identity().public_key;
    let initial_sender_idx =
        wait_any_discovery_binding(&stable_nodes, &bob_id, &initial_bob_pubkey).await;
    let (initial_sender_id, initial_sender) = stable_nodes[initial_sender_idx];
    let initial_sender_id = initial_sender_id.clone();

    initial_sender
        .send_signaling(
            &bob_id,
            MessageContent::Data(offer_signal(&initial_sender_id, &bob_id, &room_id)),
        )
        .await
        .unwrap();
    let initial_offer = recv_matching_signaling_data(&mut bob_rx, |data| {
        data.sender_id == initial_sender_id
            && data.receiver_id == bob_id
            && data.signaling_type == SignalingType::Offer
    })
    .await;
    assert_eq!(initial_offer.receiver_id, bob_id);

    bob.close().await.unwrap();
    drain_receiver(&mut alice_rx).await;
    drain_receiver(&mut bob_rx).await;
    drain_receiver(&mut carol_rx).await;
    drain_receiver(&mut dave_rx).await;

    let rejoined_bob = WasmNostrSignaler::new(bob_id.clone(), live_config(&relay_url));
    let (rejoined_bob_tx, mut rejoined_bob_rx) = mpsc::unbounded_channel();
    rejoined_bob.connect(rejoined_bob_tx).await.unwrap();
    rejoined_bob
        .send_signaling(
            &NodeId::broadcast(),
            MessageContent::Data(discovery_signal(&bob_id, &room_id)),
        )
        .await
        .unwrap();

    let rejoined_bob_pubkey = rejoined_bob.current_identity().public_key;
    assert_ne!(initial_bob_pubkey, rejoined_bob_pubkey);
    let rejoin_sender_idx =
        wait_any_discovery_binding(&stable_nodes, &bob_id, &rejoined_bob_pubkey).await;
    let (rejoin_sender_id, rejoin_sender) = stable_nodes[rejoin_sender_idx];
    let rejoin_sender_id = rejoin_sender_id.clone();

    rejoin_sender
        .send_signaling(
            &bob_id,
            MessageContent::Data(offer_signal(&rejoin_sender_id, &bob_id, &room_id)),
        )
        .await
        .unwrap();
    let rejoin_offer = recv_matching_signaling_data(&mut rejoined_bob_rx, |data| {
        data.sender_id == rejoin_sender_id
            && data.receiver_id == bob_id
            && data.signaling_type == SignalingType::Offer
    })
    .await;
    assert_eq!(rejoin_offer.receiver_id, bob_id);
    assert_eq!(rejoin_offer.signaling_type, SignalingType::Offer);

    alice.close().await.unwrap();
    carol.close().await.unwrap();
    dave.close().await.unwrap();
    rejoined_bob.close().await.unwrap();
}

#[wasm_bindgen_test(async)]
async fn nostr_live_relay_six_nodes_connect_when_configured() {
    if env_string("MIST_NOSTR_SIX_NODES").is_none()
        || env_string("MIST_NOSTR_MIXED_SIX_NODES").is_some()
    {
        return;
    }
    let Some(relay_url) = env_string("MIST_NOSTR_RELAY_URL") else {
        return;
    };

    let room_id = format!("wasm-nostr-six-room-{}", random_subscription_id());
    let ids: Vec<NodeId> = (b'A'..=b'F')
        .map(|label| NodeId(format!("wasm-node-{}-{}", label as char, room_id)))
        .collect();
    let mut nodes = Vec::new();
    let mut receivers = Vec::new();

    for id in &ids {
        let node = WasmNostrSignaler::new(id.clone(), live_config(&relay_url));
        let (tx, rx) = mpsc::unbounded_channel();
        node.connect(tx).await.unwrap();
        receivers.push((id.clone(), rx));
        nodes.push(node);
    }

    let mut edges = std::collections::BTreeSet::new();
    for (id, node) in ids.iter().zip(nodes.iter()) {
        node.send_signaling(
            &NodeId::broadcast(),
            MessageContent::Data(discovery_signal(id, &room_id)),
        )
        .await
        .unwrap();
        TimeoutFuture::new(150).await;
        drain_request_edges(&mut receivers, &mut edges).await;
    }
    TimeoutFuture::new(250).await;
    drain_request_edges(&mut receivers, &mut edges).await;

    assert!(
        graph_is_connected(&ids, &edges),
        "six-node WASM Nostr signaling graph should be connected, got edges: {:?}",
        edges
    );
    assert!(
        edges.len() <= nodes.len() * 2,
        "six-node WASM Nostr bootstrap should stay sparse, got {} edges: {:?}",
        edges.len(),
        edges
    );

    for node in nodes {
        node.close().await.unwrap();
    }
}

#[wasm_bindgen_test(async)]
async fn nostr_live_relay_mixed_six_wasm_nodes_connect_to_native_when_configured() {
    if env_string("MIST_NOSTR_MIXED_SIX_NODES").is_none() {
        return;
    }
    let Some(relay_url) = env_string("MIST_NOSTR_RELAY_URL") else {
        return;
    };

    let room_id = env_string("MIST_NOSTR_ROOM_ID")
        .unwrap_or_else(|| format!("wasm-native-nostr-six-room-{}", random_subscription_id()));
    let ids: Vec<NodeId> = (b'A'..=b'F')
        .map(|label| NodeId(format!("wasm-node-{}-{}", label as char, room_id)))
        .collect();
    let mut nodes = Vec::new();
    let mut receivers = Vec::new();

    for id in &ids {
        let node = WasmNostrSignaler::new(id.clone(), live_config(&relay_url));
        let (tx, rx) = mpsc::unbounded_channel();
        node.connect(tx).await.unwrap();
        receivers.push((id.clone(), rx));
        nodes.push(node);
    }

    TimeoutFuture::new(3_500).await;

    let mut edges = std::collections::BTreeSet::new();
    for _round in 0..2 {
        for (id, node) in ids.iter().zip(nodes.iter()) {
            node.send_signaling(
                &NodeId::broadcast(),
                MessageContent::Data(discovery_signal(id, &room_id)),
            )
            .await
            .unwrap();
            TimeoutFuture::new(175).await;
            drain_request_edges(&mut receivers, &mut edges).await;
        }
        TimeoutFuture::new(500).await;
        drain_request_edges(&mut receivers, &mut edges).await;
    }

    TimeoutFuture::new(1_000).await;
    drain_request_edges(&mut receivers, &mut edges).await;

    assert!(
        graph_is_connected(&ids, &edges),
        "mixed six-node WASM Nostr signaling graph should be connected, got edges: {:?}",
        edges
    );
    assert!(
        has_cross_edge(&edges, "wasm-node-", "native-node-"),
        "WASM nodes should receive at least one request edge from native nodes, got edges: {:?}",
        edges
    );

    for node in nodes {
        node.close().await.unwrap();
    }
}

#[wasm_bindgen_test(async)]
async fn nostr_live_relay_reconnects_same_instance_rank_edge_when_configured() {
    if env_string("MIST_NOSTR_SIX_NODES").is_some()
        || env_string("MIST_NOSTR_MIXED_SIX_NODES").is_some()
    {
        return;
    }
    let Some(relay_url) = env_string("MIST_NOSTR_RELAY_URL") else {
        return;
    };

    let room_id = format!("wasm-nostr-rejoin-rank-room-{}", random_subscription_id());
    let alice_id = NodeId(format!("wasm-alice-{room_id}"));
    let bob_id = NodeId(format!("wasm-bob-{room_id}"));
    let alice = WasmNostrSignaler::new(alice_id.clone(), live_config(&relay_url));
    let bob = WasmNostrSignaler::new(bob_id.clone(), live_config(&relay_url));
    let (alice_tx, alice_rx) = mpsc::unbounded_channel();
    let (bob_tx, bob_rx) = mpsc::unbounded_channel();
    alice.connect(alice_tx).await.unwrap();
    bob.connect(bob_tx).await.unwrap();
    let mut receivers = vec![(alice_id.clone(), alice_rx), (bob_id.clone(), bob_rx)];
    let expected_edge = ordered_edge(&alice_id, &bob_id);

    assert!(
        discover_until_edge(
            &[(&alice_id, &alice), (&bob_id, &bob)],
            &mut receivers,
            &room_id,
            &expected_edge,
            3,
        )
        .await,
        "alice and bob should form a signaling edge on first connect",
    );

    // Bob disconnects, then reconnects on the SAME instance (same pubkey),
    // which is the case where the surviving peer would otherwise suppress
    // the duplicate request. The newer joined_at must trigger a re-request.
    bob.close().await.unwrap();
    TimeoutFuture::new(500).await;
    receivers.pop();

    let (bob_tx2, bob_rx2) = mpsc::unbounded_channel();
    bob.connect(bob_tx2).await.unwrap();
    receivers.push((bob_id.clone(), bob_rx2));
    TimeoutFuture::new(500).await;

    assert!(
        discover_until_edge(
            &[(&alice_id, &alice), (&bob_id, &bob)],
            &mut receivers,
            &room_id,
            &expected_edge,
            4,
        )
        .await,
        "alice and bob should re-form a signaling edge after bob reconnects on the same instance",
    );

    alice.close().await.unwrap();
    bob.close().await.unwrap();
}

async fn discover_until_edge(
    nodes: &[(&NodeId, &WasmNostrSignaler)],
    receivers: &mut [(NodeId, mpsc::UnboundedReceiver<MessageContent>)],
    room_id: &str,
    edge: &(String, String),
    rounds: usize,
) -> bool {
    let mut edges = std::collections::BTreeSet::new();
    for _ in 0..rounds {
        for (id, node) in nodes {
            node.send_signaling(
                &NodeId::broadcast(),
                MessageContent::Data(discovery_signal(id, room_id)),
            )
            .await
            .unwrap();
            TimeoutFuture::new(175).await;
            drain_request_edges(receivers, &mut edges).await;
        }
        if edges.contains(edge) {
            return true;
        }
    }
    TimeoutFuture::new(500).await;
    drain_request_edges(receivers, &mut edges).await;
    edges.contains(edge)
}

fn live_config(relay_url: &str) -> NostrSignalingConfig {
    NostrSignalingConfig {
        relays: vec![relay_url.to_string()],
        relay_list_url: None,
        ttl_seconds: 60,
        ..NostrSignalingConfig::default()
    }
}

fn offer_signal(sender_id: &NodeId, receiver_id: &NodeId, room_id: &str) -> SignalingData {
    SignalingData {
        sender_id: sender_id.clone(),
        receiver_id: receiver_id.clone(),
        room_id: room_id.to_string(),
        data: "v=0\r\ns=mistlib-wasm-nostr-live".to_string(),
        signaling_type: SignalingType::Offer,
    }
}

fn discovery_signal(sender_id: &NodeId, room_id: &str) -> SignalingData {
    SignalingData {
        sender_id: sender_id.clone(),
        receiver_id: NodeId::broadcast(),
        room_id: room_id.to_string(),
        data: String::new(),
        signaling_type: SignalingType::Request,
    }
}

async fn wait_any_discovery_binding(
    signalers: &[(&NodeId, &WasmNostrSignaler)],
    node_id: &NodeId,
    pubkey: &str,
) -> usize {
    for _ in 0..60 {
        for (idx, (_, signaler)) in signalers.iter().enumerate() {
            if signaler
                .discovery_table
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pubkey_for_node(node_id)
                .as_deref()
                == Some(pubkey)
            {
                return idx;
            }
        }
        TimeoutFuture::new(50).await;
    }
    panic!("timed out waiting for any WASM Nostr discovery binding");
}

async fn wait_discovery_binding(signaler: &WasmNostrSignaler, node_id: &NodeId, pubkey: &str) {
    for _ in 0..60 {
        if signaler
            .discovery_table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pubkey_for_node(node_id)
            .as_deref()
            == Some(pubkey)
        {
            return;
        }
        TimeoutFuture::new(50).await;
    }
    panic!("timed out waiting for WASM Nostr discovery binding");
}

async fn drain_receiver(rx: &mut mpsc::UnboundedReceiver<MessageContent>) {
    loop {
        let receive = rx.recv();
        let timeout = TimeoutFuture::new(25);
        match select(Box::pin(receive), Box::pin(timeout)).await {
            Either::Left((Some(_), _)) => {}
            Either::Left((None, _)) | Either::Right((_, _)) => break,
        }
    }
}

async fn recv_matching_signaling_data(
    rx: &mut mpsc::UnboundedReceiver<MessageContent>,
    matches: impl Fn(&SignalingData) -> bool,
) -> SignalingData {
    for _ in 0..200 {
        let receive = rx.recv();
        let timeout = TimeoutFuture::new(25);
        match select(Box::pin(receive), Box::pin(timeout)).await {
            Either::Left((Some(MessageContent::Data(data)), _)) if matches(&data) => return data,
            Either::Left((Some(_), _)) | Either::Right((_, _)) => {}
            Either::Left((None, _)) => panic!("incoming signaling channel closed"),
        }
    }
    panic!("timed out waiting for matching WASM Nostr signaling data");
}

async fn recv_signaling_data(rx: &mut mpsc::UnboundedReceiver<MessageContent>) -> SignalingData {
    let receive = rx.recv();
    let timeout = TimeoutFuture::new(5_000);
    match select(Box::pin(receive), Box::pin(timeout)).await {
        Either::Left((Some(MessageContent::Data(data)), _)) => data,
        Either::Left((Some(_), _)) => panic!("received non-data signaling message"),
        Either::Left((None, _)) => panic!("incoming signaling channel closed"),
        Either::Right((_, _)) => panic!("timed out waiting for WASM Nostr signaling data"),
    }
}

async fn drain_request_edges(
    receivers: &mut [(NodeId, mpsc::UnboundedReceiver<MessageContent>)],
    edges: &mut std::collections::BTreeSet<(String, String)>,
) {
    for (receiver_id, rx) in receivers.iter_mut() {
        loop {
            let receive = rx.recv();
            let timeout = TimeoutFuture::new(25);
            match select(Box::pin(receive), Box::pin(timeout)).await {
                Either::Left((Some(MessageContent::Data(data)), _))
                    if data.signaling_type == SignalingType::Request =>
                {
                    edges.insert(ordered_edge(&data.sender_id, receiver_id));
                }
                Either::Left((Some(_), _)) => {}
                Either::Left((None, _)) | Either::Right((_, _)) => break,
            }
        }
    }
}

fn ordered_edge(left: &NodeId, right: &NodeId) -> (String, String) {
    if left.0 <= right.0 {
        (left.0.clone(), right.0.clone())
    } else {
        (right.0.clone(), left.0.clone())
    }
}

fn graph_is_connected(
    ids: &[NodeId],
    edges: &std::collections::BTreeSet<(String, String)>,
) -> bool {
    let Some(first) = ids.first() else {
        return true;
    };
    let mut seen = std::collections::BTreeSet::from([first.0.clone()]);
    let mut changed = true;
    while changed {
        changed = false;
        for (left, right) in edges {
            if seen.contains(left) && seen.insert(right.clone()) {
                changed = true;
            }
            if seen.contains(right) && seen.insert(left.clone()) {
                changed = true;
            }
        }
    }
    ids.iter().all(|id| seen.contains(&id.0))
}

fn has_cross_edge(
    edges: &std::collections::BTreeSet<(String, String)>,
    local_prefix: &str,
    remote_prefix: &str,
) -> bool {
    edges.iter().any(|(left, right)| {
        (left.starts_with(local_prefix) && right.starts_with(remote_prefix))
            || (left.starts_with(remote_prefix) && right.starts_with(local_prefix))
    })
}

fn env_string(name: &str) -> Option<String> {
    let global = js_sys::global();
    let process = Reflect::get(&global, &JsValue::from_str("process")).ok()?;
    let env = Reflect::get(&process, &JsValue::from_str("env")).ok()?;
    Reflect::get(&env, &JsValue::from_str(name))
        .ok()
        .and_then(|value| value.as_string())
        .filter(|value| !value.is_empty())
}
