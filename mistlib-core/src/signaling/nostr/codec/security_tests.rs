use super::super::limits::{MAX_NOSTR_EVENT_CONTENT_CHARS, MAX_NOSTR_SIGNALING_PLAINTEXT_BYTES};
use super::*;
use crate::config::NostrSignalingConfig;
use crate::signaling::nostr::{DiscoveryTable, InvitePskCrypto, NostrCrypto, SignalingSecretKey};
use crate::signaling::SignalingType;

fn config() -> (NostrCodecConfig, InvitePskCrypto) {
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
    (codec, crypto)
}

#[test]
fn message_event_round_trips_for_normalized_xonly_keys() {
    let (codec, crypto) = config();
    for receiver_byte in 2u8..16 {
        let alice = TemporarySignalingIdentity::from_secret_key(
            SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
        );
        let bob = TemporarySignalingIdentity::from_secret_key(
            SignalingSecretKey::from_bytes_for_tests([receiver_byte; 32]),
        );
        let data = SignalingData {
            sender_id: NodeId("alice".to_string()),
            receiver_id: NodeId("bob".to_string()),
            room_id: "secret-room".to_string(),
            data: format!("probe-{receiver_byte}"),
            signaling_type: SignalingType::Offer,
        };

        let event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
        let decoded = decode_message_event(
            &codec,
            &crypto,
            &bob,
            &NodeId("bob".to_string()),
            &event,
            "secret-room",
        )
        .unwrap();
        assert_eq!(decoded.data.data, data.data);
    }
}

#[test]
fn invite_holder_without_receiver_secret_cannot_decrypt_message() {
    let (codec, crypto) = config();
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let bob = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([2u8; 32]),
    );
    let attacker = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([3u8; 32]),
    );
    let data = SignalingData {
        sender_id: NodeId("alice".to_string()),
        receiver_id: NodeId("bob".to_string()),
        room_id: "secret-room".to_string(),
        data: "v=0\r\nsdp".to_string(),
        signaling_type: SignalingType::Offer,
    };

    let event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();

    assert!(crypto
        .decrypt(&attacker, &alice.public_key, &event.content)
        .is_err());
}

#[test]
fn wrong_invite_secret_cannot_decrypt_message() {
    let (codec, crypto) = config();
    let wrong_crypto = InvitePskCrypto::new("other-salt", "other-invite");
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

    let err = decode_message_event(
        &codec,
        &wrong_crypto,
        &bob,
        &NodeId("bob".to_string()),
        &event,
        "secret-room",
    )
    .unwrap_err();

    assert!(format!("{err:?}").contains("invalid encrypted Nostr payload"));
}

#[test]
fn oversized_message_content_is_rejected_before_decrypt() {
    let (codec, crypto) = config();
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
    let mut event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    event.content = "A".repeat(MAX_NOSTR_EVENT_CONTENT_CHARS + 1);
    event.refresh_id();
    event.sig = crypto.sign_event(&alice, &event).unwrap();

    let err = decode_message_event(
        &codec,
        &crypto,
        &bob,
        &NodeId("bob".to_string()),
        &event,
        "secret-room",
    )
    .unwrap_err();

    assert!(format!("{err:?}").contains("content is too large"));
}

#[test]
fn oversized_plaintext_payload_is_rejected_before_encrypt() {
    let (codec, crypto) = config();
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
        data: "x".repeat(MAX_NOSTR_SIGNALING_PLAINTEXT_BYTES + 1),
        signaling_type: SignalingType::Offer,
    };

    assert!(build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).is_err());
}

#[test]
fn message_room_scope_mismatch_is_rejected() {
    let (codec, crypto) = config();
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
        data: "v=0
sdp"
        .to_string(),
        signaling_type: SignalingType::Offer,
    };
    let mut event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    for tag in &mut event.tags {
        if tag.first().map(String::as_str) == Some(TAG_INVITE_SCOPE) {
            tag[1] = "00".repeat(32);
        }
    }
    event.refresh_id();
    event.sig = crypto.sign_event(&alice, &event).unwrap();

    assert!(decode_message_event(
        &codec,
        &crypto,
        &bob,
        &NodeId("bob".to_string()),
        &event,
        "secret-room",
    )
    .is_err());
}

#[test]
fn legacy_message_event_without_p_tag_still_decodes() {
    // A sender that predates the `p` tag scheme omits it entirely. Decode
    // must tolerate its absence (falls back to the room-mailbox path) rather
    // than requiring it, so old senders keep working against new receivers.
    let (codec, crypto) = config();
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
    let mut event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    assert!(
        event.tag_value(TAG_P).is_some(),
        "sanity: new builds do tag p"
    );
    event
        .tags
        .retain(|tag| tag.first().map(String::as_str) != Some(TAG_P));
    event.refresh_id();
    event.sig = crypto.sign_event(&alice, &event).unwrap();
    assert!(event.tag_value(TAG_P).is_none());

    let decoded = decode_message_event(
        &codec,
        &crypto,
        &bob,
        &NodeId("bob".to_string()),
        &event,
        "secret-room",
    )
    .unwrap();

    assert_eq!(decoded.data.data, data.data);
}

#[test]
fn message_addressed_to_a_different_peer_is_rejected() {
    // Defense in depth: even if a misbehaving/legacy relay delivers an event
    // whose `p` tag names someone else, decode must not treat us as the
    // recipient (a correctly filtering relay would never deliver it to us
    // at all, since our subscription only asks for our own pubkey or the
    // broadcast sentinel).
    let (codec, crypto) = config();
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let bob = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([2u8; 32]),
    );
    let charlie = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([3u8; 32]),
    );
    let data = SignalingData {
        sender_id: NodeId("alice".to_string()),
        receiver_id: NodeId("bob".to_string()),
        room_id: "secret-room".to_string(),
        data: "v=0\r\nsdp".to_string(),
        signaling_type: SignalingType::Offer,
    };
    let event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    assert_eq!(event.tag_value(TAG_P), Some(bob.public_key.as_str()));

    let err = decode_message_event(
        &codec,
        &crypto,
        &charlie,
        &NodeId("charlie".to_string()),
        &event,
        "secret-room",
    )
    .unwrap_err();

    assert!(format!("{err:?}").contains("receiver pubkey mismatch"));
}

#[test]
fn tampered_ciphertext_is_rejected_after_resign() {
    let (codec, crypto) = config();
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
    let mut event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    let tamper_at = event.content.len() / 2;
    let original = event.content.as_bytes()[tamper_at] as char;
    let replacement = if original == 'A' { "B" } else { "A" };
    event
        .content
        .replace_range(tamper_at..tamper_at + 1, replacement);
    event.refresh_id();
    event.sig = crypto.sign_event(&alice, &event).unwrap();

    let err = decode_message_event(
        &codec,
        &crypto,
        &bob,
        &NodeId("bob".to_string()),
        &event,
        "secret-room",
    )
    .unwrap_err();

    assert!(format!("{err:?}").contains("invalid encrypted Nostr payload"));
}

#[test]
fn ciphertext_is_bound_to_signed_sender_pubkey() {
    let (codec, crypto) = config();
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let bob = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([2u8; 32]),
    );
    let mallory = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([3u8; 32]),
    );
    let data = SignalingData {
        sender_id: NodeId("alice".to_string()),
        receiver_id: NodeId("bob".to_string()),
        room_id: "secret-room".to_string(),
        data: "v=0\r\nsdp".to_string(),
        signaling_type: SignalingType::Offer,
    };
    let mut event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    event.pubkey = mallory.public_key.clone();
    event.refresh_id();
    event.sig = crypto.sign_event(&mallory, &event).unwrap();

    let err = decode_message_event(
        &codec,
        &crypto,
        &bob,
        &NodeId("bob".to_string()),
        &event,
        "secret-room",
    )
    .unwrap_err();

    assert!(format!("{err:?}").contains("invalid encrypted Nostr payload"));
}

#[test]
fn decrypted_payload_room_mismatch_is_rejected() {
    let (codec, crypto) = config();
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
    let mut event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    let other_room_scope = codec.current_room_scope("other-room");
    for tag in &mut event.tags {
        if tag.first().map(String::as_str) == Some(TAG_INVITE_SCOPE) {
            tag[1] = other_room_scope.clone();
        }
    }
    event.refresh_id();
    event.sig = crypto.sign_event(&alice, &event).unwrap();

    let err = decode_message_event(
        &codec,
        &crypto,
        &bob,
        &NodeId("bob".to_string()),
        &event,
        "other-room",
    )
    .unwrap_err();

    assert!(format!("{err:?}").contains("room mismatch"));
}

#[test]
fn repeated_message_encryption_produces_unique_ciphertexts() {
    let (codec, crypto) = config();
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
        data: "same plaintext".to_string(),
        signaling_type: SignalingType::Offer,
    };
    let mut contents = std::collections::BTreeSet::new();

    for _ in 0..16 {
        let event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
        assert!(contents.insert(event.content));
    }
}

#[test]
fn discovery_with_same_d_tag_but_invalid_proof_is_rejected() {
    let (codec, crypto) = config();
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let mallory = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([3u8; 32]),
    );
    let valid = build_discovery_event(&codec, &crypto, &alice, "secret-room").unwrap();
    let mut polluted = NostrEvent::unsigned(
        mallory.public_key.clone(),
        codec.discovery_kind,
        valid.tags.clone(),
        String::new(),
    );
    polluted.sig = crypto.sign_event(&mallory, &polluted).unwrap();

    let err = decode_discovery_event(&codec, &crypto, &polluted, "secret-room").unwrap_err();

    assert!(format!("{err:?}").contains("discovery proof"));
}

#[test]
fn discovery_without_proof_is_rejected_even_with_matching_d_tag() {
    let (codec, crypto) = config();
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let mut event = build_discovery_event(&codec, &crypto, &alice, "secret-room").unwrap();
    event
        .tags
        .retain(|tag| tag.first().map(String::as_str) != Some(TAG_DISCOVERY_PROOF));
    event.refresh_id();
    event.sig = crypto.sign_event(&alice, &event).unwrap();

    let err = decode_discovery_event(&codec, &crypto, &event, "secret-room").unwrap_err();

    assert!(format!("{err:?}").contains("discovery proof"));
}

#[test]
fn discovery_table_rejects_node_id_pubkey_rebinding() {
    let mut table = DiscoveryTable::default();
    let node = NodeId("alice".to_string());
    let first = "01".repeat(32);
    let second = "02".repeat(32);

    assert!(!table
        .bind_node_checked(node.clone(), first.clone(), u64::MAX)
        .unwrap());
    assert!(table
        .bind_node_checked(node.clone(), first, u64::MAX)
        .unwrap());
    assert!(table.bind_node_checked(node, second, u64::MAX).is_err());
}

#[test]
fn discovery_table_does_not_rollback_pubkey_expiration() {
    let mut table = DiscoveryTable::default();
    let pubkey = "01".repeat(32);

    let later = now_unix_seconds().saturating_add(200);
    let earlier = later.saturating_sub(100);

    table.insert_pubkey(pubkey.clone(), later);
    table.insert_pubkey(pubkey.clone(), earlier);

    assert_eq!(table.expires_at_for_pubkey(&pubkey), Some(later));
}
