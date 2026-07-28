use super::*;

#[test]
fn discovery_event_round_trips_without_node_id() {
    let (_raw, codec, crypto) = config();
    let identity = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );

    let event = build_discovery_event(&codec, &crypto, &identity, "secret-room").unwrap();
    let event_json = serde_json::to_string(&event).unwrap();
    assert!(!event_json.contains("alice"));
    assert!(!event_json.contains("secret-room"));
    assert!(!event_json.contains("webrtc"));
    assert!(!event_json.contains("mistlib"));
    assert!(!event_json.contains("room"));
    assert!(!event_json.contains("joined_at"));
    assert!(event.tag_value(TAG_NONCE).is_some());

    let second = build_discovery_event(&codec, &crypto, &identity, "secret-room").unwrap();
    assert_ne!(event.id, second.id);

    let decoded = decode_discovery_event(&codec, &crypto, &event, "secret-room").unwrap();
    assert_eq!(decoded.signaling_pubkey, identity.public_key);
    assert_eq!(
        decoded.topology_rank,
        codec.topology_rank("secret-room", &identity.public_key)
    );
}

#[test]
fn message_event_round_trips_and_hides_plaintext() {
    let (_raw, codec, crypto) = config();
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let bob = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([2u8; 32]),
    );
    let data = SignalingData {
        sender_id: NodeId("alice".to_string()),
        receiver_id: NodeId("bob".to_string()),
        room_id: "secret-room".to_string(),
        data: "v=0\r\nsdp".to_string(),
        signaling_type: SignalingType::Offer,
    };

    let event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    let event_json = serde_json::to_string(&event).unwrap();
    assert!(!event.content.contains("v=0"));
    assert!(!event.content.contains("mistpsk1"));
    assert!(!event_json.contains("alice"));
    assert!(!event_json.contains("bob"));
    assert!(!event_json.contains("secret-room"));
    assert!(!event_json.contains("mistlib"));
    assert!(!event_json.contains("room"));
    // Directed messages tag the real recipient pubkey (standard Nostr `p`
    // tag targeting) so the relay can filter without decrypting; this is an
    // intentional, documented metadata tradeoff for O(1) fan-out (see
    // docs/architecture/SIGNALING.md).
    assert_eq!(event.tag_value(TAG_P), Some(bob.public_key.as_str()));
    let decoded = decode_message_event(
        &codec,
        &crypto,
        &bob,
        &NodeId("bob".to_string()),
        &event,
        "secret-room",
    )
    .unwrap();
    assert_eq!(decoded.sender_pubkey, alice.public_key);
    assert_eq!(decoded.data.sender_id, data.sender_id);
    assert_eq!(decoded.data.data, data.data);
    assert_eq!(decoded.sequence, Some(1));
    assert_eq!(decoded.message_id.as_deref().unwrap().len(), 32);
}

#[test]
fn message_event_sequence_round_trips() {
    let (_raw, codec, crypto) = config();
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let bob = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([2u8; 32]),
    );
    let data = SignalingData {
        sender_id: NodeId("alice".to_string()),
        receiver_id: NodeId("bob".to_string()),
        room_id: "secret-room".to_string(),
        data: "v=0\r\nsdp".to_string(),
        signaling_type: SignalingType::Offer,
    };

    let event =
        build_message_event_with_sequence(&codec, &crypto, &alice, &bob.public_key, &data, 7)
            .unwrap();
    let decoded = decode_message_event(
        &codec,
        &crypto,
        &bob,
        &NodeId("bob".to_string()),
        &event,
        "secret-room",
    )
    .unwrap();

    assert_eq!(decoded.sequence, Some(7));
    assert_eq!(decoded.message_id.as_deref().unwrap().len(), 32);
    assert_eq!(decoded.data, data);
}

#[test]
fn message_event_sender_joined_at_round_trips() {
    let (_raw, codec, crypto) = config();
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let bob = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([2u8; 32]),
    );
    let data = SignalingData {
        sender_id: NodeId("alice".to_string()),
        receiver_id: NodeId("bob".to_string()),
        room_id: "secret-room".to_string(),
        data: "v=0\r\nsdp".to_string(),
        signaling_type: SignalingType::Offer,
    };

    let event = build_message_event_with_sequence_and_joined_at(
        &codec,
        &crypto,
        &alice,
        &bob.public_key,
        &data,
        8,
        Some(1234),
    )
    .unwrap();
    let decoded = decode_message_event(
        &codec,
        &crypto,
        &bob,
        &NodeId("bob".to_string()),
        &event,
        "secret-room",
    )
    .unwrap();

    assert_eq!(decoded.sequence, Some(8));
    assert_eq!(decoded.sender_joined_at, Some(1234));
    assert_eq!(decoded.data, data);
}

#[test]
fn zero_message_sequence_is_rejected() {
    let (_raw, codec, crypto) = config();
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let bob = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([2u8; 32]),
    );
    let data = SignalingData {
        sender_id: NodeId("alice".to_string()),
        receiver_id: NodeId("bob".to_string()),
        room_id: "secret-room".to_string(),
        data: "v=0\r\nsdp".to_string(),
        signaling_type: SignalingType::Offer,
    };

    assert!(
        build_message_event_with_sequence(&codec, &crypto, &alice, &bob.public_key, &data, 0)
            .is_err()
    );
}
