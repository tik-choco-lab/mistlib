use super::*;

#[tokio::test]
async fn duplicate_relay_delivery_is_deduped_by_event_id() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    let peer = NostrSignaler::new(NodeId("bob".to_string()), config());
    let (tx, mut rx) = mpsc::channel(4);
    signaler.set_room_id("room-a").await.unwrap();
    signaler
        .requested_pubkeys
        .lock()
        .await
        .insert(peer.identity.public_key.clone());
    let payload = data(
        &NodeId("bob".to_string()),
        &NodeId("alice".to_string()),
        "room-a",
        SignalingType::Offer,
    );
    let event = build_message_event_with_sequence(
        &peer.codec_config,
        &peer.crypto,
        &peer.identity,
        &signaler.identity.public_key,
        &payload,
        1,
    )
    .unwrap();

    signaler
        .process_event(event.clone(), tx.clone())
        .await
        .unwrap();
    assert_eq!(recv_available(&mut rx).await.unwrap(), payload);

    signaler.process_event(event, tx).await.unwrap();
    assert!(recv_available(&mut rx).await.is_none());
}

#[tokio::test]
async fn replayed_payload_with_new_event_id_is_deduped_by_message_id() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    let peer = NostrSignaler::new(NodeId("bob".to_string()), config());
    let (tx, mut rx) = mpsc::channel(4);
    signaler.set_room_id("room-a").await.unwrap();
    signaler
        .requested_pubkeys
        .lock()
        .await
        .insert(peer.identity.public_key.clone());
    let payload = data(
        &NodeId("bob".to_string()),
        &NodeId("alice".to_string()),
        "room-a",
        SignalingType::Offer,
    );
    let event = build_message_event_with_sequence(
        &peer.codec_config,
        &peer.crypto,
        &peer.identity,
        &signaler.identity.public_key,
        &payload,
        1,
    )
    .unwrap();
    let mut replay = event.clone();
    replay.created_at = replay.created_at.saturating_add(1);
    replay.refresh_id();
    replay.sig = peer.crypto.sign_event(&peer.identity, &replay).unwrap();
    assert_ne!(event.id, replay.id);

    signaler.process_event(event, tx.clone()).await.unwrap();
    assert_eq!(recv_available(&mut rx).await.unwrap(), payload);

    signaler.process_event(replay, tx).await.unwrap();
    assert!(recv_available(&mut rx).await.is_none());
}

#[tokio::test]
async fn older_message_sequence_within_reorder_window_is_still_delivered() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    let peer = NostrSignaler::new(NodeId("bob".to_string()), config());
    let (tx, mut rx) = mpsc::channel(4);
    signaler.set_room_id("room-a").await.unwrap();
    signaler
        .requested_pubkeys
        .lock()
        .await
        .insert(peer.identity.public_key.clone());
    let mut older = data(
        &NodeId("bob".to_string()),
        &NodeId("alice".to_string()),
        "room-a",
        SignalingType::Candidate,
    );
    older.data = "older".to_string();
    let mut newer = older.clone();
    newer.data = "newer".to_string();
    let newer_event = build_message_event_with_sequence(
        &peer.codec_config,
        &peer.crypto,
        &peer.identity,
        &signaler.identity.public_key,
        &newer,
        2,
    )
    .unwrap();
    let older_event = build_message_event_with_sequence(
        &peer.codec_config,
        &peer.crypto,
        &peer.identity,
        &signaler.identity.public_key,
        &older,
        1,
    )
    .unwrap();

    signaler
        .process_event(newer_event, tx.clone())
        .await
        .unwrap();
    assert_eq!(recv_available(&mut rx).await.unwrap(), newer);

    // Sequence 1 trails the high-water mark (2) by only one position, well
    // within NOSTR_SEQUENCE_REORDER_WINDOW. This models a slow-to-encrypt
    // Offer (sequence 1) losing the wire race to a cheap Candidate (sequence
    // 2) that was assigned later: it must still be delivered, not silently
    // discarded, or the peer's connect attempt dies with no diagnostic.
    signaler.process_event(older_event, tx).await.unwrap();
    assert_eq!(recv_available(&mut rx).await.unwrap(), older);
}

#[tokio::test]
async fn message_sequence_beyond_reorder_window_is_dropped() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    let peer = NostrSignaler::new(NodeId("bob".to_string()), config());
    let (tx, mut rx) = mpsc::channel(4);
    signaler.set_room_id("room-a").await.unwrap();
    signaler
        .requested_pubkeys
        .lock()
        .await
        .insert(peer.identity.public_key.clone());
    let mut ancient = data(
        &NodeId("bob".to_string()),
        &NodeId("alice".to_string()),
        "room-a",
        SignalingType::Candidate,
    );
    ancient.data = "ancient".to_string();
    let mut far_newer = ancient.clone();
    far_newer.data = "far-newer".to_string();
    let far_ahead_sequence = NOSTR_SEQUENCE_REORDER_WINDOW + 10;
    let far_newer_event = build_message_event_with_sequence(
        &peer.codec_config,
        &peer.crypto,
        &peer.identity,
        &signaler.identity.public_key,
        &far_newer,
        far_ahead_sequence,
    )
    .unwrap();
    let ancient_event = build_message_event_with_sequence(
        &peer.codec_config,
        &peer.crypto,
        &peer.identity,
        &signaler.identity.public_key,
        &ancient,
        1,
    )
    .unwrap();

    signaler
        .process_event(far_newer_event, tx.clone())
        .await
        .unwrap();
    assert_eq!(recv_available(&mut rx).await.unwrap(), far_newer);

    // Sequence 1 trails the high-water mark by more than the reorder window,
    // so it is genuinely stale (not a legitimate reorder) and is dropped.
    signaler.process_event(ancient_event, tx).await.unwrap();
    assert!(recv_available(&mut rx).await.is_none());
}
