use super::*;

#[test]
fn relay_frames_do_not_expose_invite_or_room_material() {
    let (raw, codec, crypto) = config();
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
        data: "kind 25050 encrypted payload test".to_string(),
        signaling_type: SignalingType::Offer,
    };

    let discovery = build_discovery_event(&codec, &crypto, &alice, "secret-room").unwrap();
    let message = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    let discovery_req = req_frame_json(
        &random_subscription_id(),
        &[discovery_filter(&codec, "secret-room")],
    )
    .unwrap();
    let message_req = req_frame_json(
        &random_subscription_id(),
        &[message_filter(&codec, "secret-room", &bob.public_key)],
    )
    .unwrap();
    let capture = [
        event_frame_json(&discovery).unwrap(),
        event_frame_json(&message).unwrap(),
        discovery_req,
        message_req,
    ]
    .join("\n");

    for hidden in [
        raw.invite_salt.as_str(),
        raw.invite_code.as_str(),
        "secret-room",
        "alice",
        "bob",
        "kind 25050 encrypted payload test",
        "mistlib",
        "webrtc",
        "mistpsk1",
        "room",
        "joined_at",
        "discovery",
        "messages",
    ] {
        assert!(!capture.contains(hidden), "{hidden} leaked in relay frame");
    }
}

#[test]
fn subscription_ids_are_random_opaque_hex() {
    let first = random_subscription_id();
    let second = random_subscription_id();

    assert_eq!(first.len(), 32);
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(first, second);
}

#[test]
fn relay_room_scope_differs_per_room() {
    let (_raw, codec, _crypto) = config();
    let first = discovery_filter(&codec, "first-room");
    let second = discovery_filter(&codec, "second-room");

    assert_ne!(
        first.tag_filters.get("#d").unwrap(),
        second.tag_filters.get("#d").unwrap()
    );
}

#[test]
fn discovery_scope_rotates_by_time_bucket() {
    let (_raw, codec, _crypto) = config();
    let current = current_rotation_bucket(codec.room_scope_rotation_seconds());

    assert_ne!(
        codec.room_scope("secret-room", current),
        codec.room_scope("secret-room", current + 1)
    );
}

#[test]
fn topology_rank_is_secret_room_scoped_material() {
    let (_raw, codec, _crypto) = config();
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );

    let first = codec.topology_rank("first-room", &alice.public_key);
    let second = codec.topology_rank("second-room", &alice.public_key);

    assert_eq!(first.len(), 64);
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(first, second);
}

#[test]
fn message_event_directed_to_known_receiver_uses_p_tag() {
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
        data: "v=0
sdp"
        .to_string(),
        signaling_type: SignalingType::Offer,
    };

    let event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    let room_scope = codec.current_room_scope(&data.room_id);
    let other_room_scope = codec.current_room_scope("other-room");
    // Bob's own subscription: message_filter is computed for the local
    // (receiving) node's pubkey.
    let bob_filter = message_filter(&codec, &data.room_id, &bob.public_key);

    assert_eq!(event.tag_value(TAG_INVITE_SCOPE), Some(room_scope.as_str()));
    assert_ne!(room_scope, other_room_scope);
    // Standard Nostr `p` tag targeting: the recipient's real signaling pubkey
    // is intentionally visible to the relay (that's the whole point — it lets
    // the relay filter without decrypting), unlike the room/invite material
    // asserted hidden above. This is the documented O(room-size) -> O(1)
    // fan-out tradeoff.
    assert_eq!(event.tag_value(TAG_P), Some(bob.public_key.as_str()));
    assert!(event.tag_value(TAG_NONCE).is_some());
    assert!(bob_filter
        .tag_filters
        .get("#d")
        .unwrap()
        .contains(&room_scope));
    assert!(bob_filter
        .tag_filters
        .get("#p")
        .unwrap()
        .contains(&bob.public_key));
}

#[test]
fn broadcast_message_uses_sentinel_p_tag_and_hides_all_pubkeys() {
    let (_raw, codec, crypto) = config();
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let bob = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([2u8; 32]),
    );
    let data = SignalingData {
        sender_id: NodeId("alice".to_string()),
        receiver_id: NodeId::broadcast(),
        room_id: "secret-room".to_string(),
        data: "v=0\r\nsdp".to_string(),
        signaling_type: SignalingType::Request,
    };

    // Broadcast messages are still unicast-encrypted to a concrete Nostr
    // pubkey (here, bob's) even though the logical `receiver_id` is unknown;
    // only the `p` tag differs from the directed case.
    let event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    let sentinel = codec.current_broadcast_sentinel(&data.room_id);
    let event_json = serde_json::to_string(&event).unwrap();

    assert_eq!(event.tag_value(TAG_P), Some(sentinel.as_str()));
    assert_ne!(event.tag_value(TAG_P), Some(bob.public_key.as_str()));
    // The event's own `pubkey` field always identifies the sender (alice) —
    // that's inherent to Nostr, not something this change touches. What
    // matters here is that the *recipient* (bob), who would otherwise appear
    // in the `p` tag for a directed message, stays hidden behind the shared
    // sentinel for a broadcast one.
    assert!(!event_json.contains(&bob.public_key));

    // Every member's own subscription filter accepts the same sentinel,
    // regardless of whose pubkey it is.
    let alice_filter = message_filter(&codec, &data.room_id, &alice.public_key);
    let bob_filter = message_filter(&codec, &data.room_id, &bob.public_key);
    assert!(alice_filter
        .tag_filters
        .get("#p")
        .unwrap()
        .contains(&sentinel));
    assert!(bob_filter
        .tag_filters
        .get("#p")
        .unwrap()
        .contains(&sentinel));
}

#[test]
fn broadcast_sentinel_differs_per_room_and_rotation_bucket_but_stable_within_one() {
    let (_raw, codec, _crypto) = config();
    let current = current_rotation_bucket(codec.room_scope_rotation_seconds());

    let first_room_a = codec.broadcast_sentinel("first-room", current);
    let second_room_a = codec.broadcast_sentinel("first-room", current);
    let first_room_b = codec.broadcast_sentinel("second-room", current);
    let next_bucket = codec.broadcast_sentinel("first-room", current + 1);

    assert_eq!(first_room_a, second_room_a, "stable within one bucket");
    assert_ne!(first_room_a, first_room_b, "unlinkable across rooms");
    assert_ne!(first_room_a, next_bucket, "rotates with the bucket");
}

#[test]
fn message_filter_p_tag_is_exactly_own_pubkey_plus_accepted_sentinels() {
    let (_raw, codec, _crypto) = config();
    let room_id = "secret-room";
    let local_pubkey = "aa".repeat(32);

    let filter = message_filter(&codec, room_id, &local_pubkey);
    let p_values = filter.tag_filters.get("#p").unwrap();
    let mut expected: Vec<String> = vec![local_pubkey.clone()];
    expected.extend(codec.accepted_broadcast_sentinels(room_id));

    assert_eq!(p_values, &expected);
    assert!(p_values.contains(&local_pubkey));
    // The existing `#d` room-scope window is untouched by this change.
    assert_eq!(
        filter.tag_filters.get("#d").unwrap(),
        &codec.accepted_room_scopes(room_id)
    );
}

/// Simulates relay-side `#p` matching without a real relay (a relay's own
/// generic tag matcher is out of scope here): a
/// message's `p` tag value must be a member of the accepted `#p` set on the
/// intended recipient's own filter for it to have been delivered.
#[test]
fn directed_message_p_tag_matches_only_the_intended_recipients_filter() {
    let (_raw, codec, crypto) = config();
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
    let event_p_tag = event.tag_value(TAG_P).unwrap();

    let bob_filter = message_filter(&codec, &data.room_id, &bob.public_key);
    let charlie_filter = message_filter(&codec, &data.room_id, &charlie.public_key);

    assert!(bob_filter
        .tag_filters
        .get("#p")
        .unwrap()
        .iter()
        .any(|v| v == event_p_tag));
    assert!(!charlie_filter
        .tag_filters
        .get("#p")
        .unwrap()
        .iter()
        .any(|v| v == event_p_tag));
}

/// Complement of the directed case above: a broadcast message's `p` tag
/// (the room's sentinel) is present in every member's own filter, so a
/// correctly filtering relay delivers it to all of them.
#[test]
fn broadcast_message_p_tag_matches_every_members_filter() {
    let (_raw, codec, crypto) = config();
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
        receiver_id: NodeId::broadcast(),
        room_id: "secret-room".to_string(),
        data: String::new(),
        signaling_type: SignalingType::Request,
    };

    let event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
    let event_p_tag = event.tag_value(TAG_P).unwrap();

    for member_pubkey in [&alice.public_key, &bob.public_key, &charlie.public_key] {
        let filter = message_filter(&codec, &data.room_id, member_pubkey);
        assert!(
            filter
                .tag_filters
                .get("#p")
                .unwrap()
                .iter()
                .any(|v| v == event_p_tag),
            "member {member_pubkey} should accept the broadcast sentinel"
        );
    }
}

#[test]
fn message_payloads_use_random_cover_size_buckets() {
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
        data: "x".to_string(),
        signaling_type: SignalingType::Offer,
    };
    let allowed_content_lengths = [1404usize, 2770, 5500, 10962];
    let mut observed_lengths = std::collections::BTreeSet::new();
    let mut first_event = None;
    let mut fixed_hex_prefix_count = 0usize;

    for _ in 0..64 {
        let event = build_message_event(&codec, &crypto, &alice, &bob.public_key, &data).unwrap();
        assert!(
            allowed_content_lengths.contains(&event.content.len()),
            "unexpected encrypted content length {}",
            event.content.len()
        );
        assert!(event
            .content
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(!event.content.chars().all(|c| c.is_ascii_hexdigit()));
        if event.content.starts_with("03") {
            fixed_hex_prefix_count += 1;
        }
        observed_lengths.insert(event.content.len());
        first_event.get_or_insert(event);
    }

    assert!(
        observed_lengths.len() > 1,
        "Nostr padding should vary cover size across messages"
    );
    assert!(
        fixed_hex_prefix_count < 64,
        "Nostr content must not use a fixed 03 hex version prefix"
    );
    let decoded = decode_message_event(
        &codec,
        &crypto,
        &bob,
        &NodeId("bob".to_string()),
        first_event.as_ref().unwrap(),
        "secret-room",
    )
    .unwrap();
    assert_eq!(decoded.data.data, data.data);
}
