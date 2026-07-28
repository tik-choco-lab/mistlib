use super::super::NostrSignaler;
use super::{config, data, recv_available};
use mistlib_core::signaling::nostr::{build_message_event, build_message_event_with_sequence};
use mistlib_core::signaling::SignalingType;
use mistlib_core::types::NodeId;
use tokio::sync::mpsc;

#[tokio::test]
async fn non_recipient_room_mailbox_message_is_ignored() {
    let alice = NostrSignaler::new(NodeId("alice".to_string()), config());
    let bob = NostrSignaler::new(NodeId("bob".to_string()), config());
    let charlie = NostrSignaler::new(NodeId("charlie".to_string()), config());
    let (tx, mut rx) = mpsc::channel(1);
    charlie.set_room_id("room-a").await.unwrap();
    let event = build_message_event(
        &alice.codec_config,
        &alice.crypto,
        &alice.identity,
        &bob.identity.public_key,
        &data(
            &NodeId("alice".to_string()),
            &NodeId("bob".to_string()),
            "room-a",
            SignalingType::Offer,
        ),
    )
    .unwrap();

    charlie.process_event(event, tx).await.unwrap();

    assert!(recv_available(&mut rx).await.is_none());
}

#[tokio::test]
async fn unexpected_candidate_from_unrequested_peer_is_dropped() {
    let alice = NostrSignaler::new(NodeId("alice".to_string()), config());
    let mallory = NostrSignaler::new(NodeId("mallory".to_string()), config());
    let (tx, mut rx) = mpsc::channel(1);
    alice.set_room_id("room-a").await.unwrap();
    let event = build_message_event_with_sequence(
        &mallory.codec_config,
        &mallory.crypto,
        &mallory.identity,
        &alice.identity.public_key,
        &data(
            &NodeId("mallory".to_string()),
            &NodeId("alice".to_string()),
            "room-a",
            SignalingType::Candidate,
        ),
        1,
    )
    .unwrap();

    alice.process_event(event, tx).await.unwrap();

    assert!(recv_available(&mut rx).await.is_none());
}

#[tokio::test]
async fn request_from_unrequested_peer_is_still_allowed_for_bootstrap() {
    let alice = NostrSignaler::new(NodeId("alice".to_string()), config());
    let bob = NostrSignaler::new(NodeId("bob".to_string()), config());
    let (tx, mut rx) = mpsc::channel(1);
    alice.set_room_id("room-a").await.unwrap();
    let (relay_tx, _relay_rx) = mpsc::channel(1);
    alice.senders.lock().await.push(relay_tx);
    let request = data(
        &NodeId("bob".to_string()),
        &NodeId::broadcast(),
        "room-a",
        SignalingType::Request,
    );
    let event = build_message_event_with_sequence(
        &bob.codec_config,
        &bob.crypto,
        &bob.identity,
        &alice.identity.public_key,
        &request,
        1,
    )
    .unwrap();

    alice.process_event(event, tx).await.unwrap();

    let received = recv_available(&mut rx).await.unwrap();
    assert_eq!(received.sender_id, NodeId("bob".to_string()));
    assert_eq!(received.receiver_id, NodeId("alice".to_string()));
    assert_eq!(received.signaling_type, SignalingType::Request);
}

#[tokio::test]
async fn invalid_room_mailbox_signature_is_rejected() {
    let alice = NostrSignaler::new(NodeId("alice".to_string()), config());
    let bob = NostrSignaler::new(NodeId("bob".to_string()), config());
    let charlie = NostrSignaler::new(NodeId("charlie".to_string()), config());
    let (tx, _rx) = mpsc::channel(1);
    charlie.set_room_id("room-a").await.unwrap();
    let mut event = build_message_event(
        &alice.codec_config,
        &alice.crypto,
        &alice.identity,
        &bob.identity.public_key,
        &data(
            &NodeId("alice".to_string()),
            &NodeId("bob".to_string()),
            "room-a",
            SignalingType::Offer,
        ),
    )
    .unwrap();
    event.sig = "invalid".to_string();

    assert!(charlie.process_event(event, tx).await.is_err());
}
