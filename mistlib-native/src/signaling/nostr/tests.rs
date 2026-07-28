use super::NostrSignaler;
use mistlib_core::config::NostrSignalingConfig;
use mistlib_core::signaling::nostr::{
    build_discovery_event, build_message_event_with_sequence, decode_discovery_event,
    decode_message_event, random_subscription_id, NostrCrypto, NostrEvent,
    NOSTR_SEQUENCE_REORDER_WINDOW,
};
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::types::NodeId;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

fn config() -> NostrSignalingConfig {
    NostrSignalingConfig {
        relays: vec![std::env::var("MIST_NOSTR_RELAY_URL")
            .unwrap_or_else(|_| "ws://127.0.0.1:7777".to_string())],
        relay_list_url: None,
        discovery_kind: 25049,
        message_kind: 25050,
        ttl_seconds: 60,
        max_clock_skew_seconds: 300,
        invite_salt: "nostr-sig-test-local-salt".to_string(),
        invite_code: "dev-invite-001".to_string(),
    }
}

struct LiveIds {
    alice: NodeId,
    bob: NodeId,
    room_id: String,
}

impl LiveIds {
    fn generate() -> Self {
        let run_id = random_subscription_id();
        Self {
            alice: NodeId(format!("alice-{run_id}")),
            bob: NodeId(format!("bob-{run_id}")),
            room_id: format!("nostr-live-room-{run_id}"),
        }
    }
}

fn data(sender: &NodeId, receiver: &NodeId, room_id: &str, kind: SignalingType) -> SignalingData {
    SignalingData {
        sender_id: sender.clone(),
        receiver_id: receiver.clone(),
        room_id: room_id.to_string(),
        data: "v=0\r\ns=mistlib-nostr-live".to_string(),
        signaling_type: kind,
    }
}

async fn recv_matching(
    rx: &mut mpsc::Receiver<MessageContent>,
    matches: impl Fn(&SignalingData) -> bool,
) -> SignalingData {
    timeout(Duration::from_secs(3), async {
        loop {
            let msg = rx.recv().await.expect("incoming channel closed");
            let MessageContent::Data(data) = msg else {
                panic!("unexpected signaling message: {msg:?}");
            };
            if matches(&data) {
                return data;
            }
        }
    })
    .await
    .expect("timed out waiting for Nostr signaling")
}

async fn recv_available(rx: &mut mpsc::Receiver<MessageContent>) -> Option<SignalingData> {
    match timeout(Duration::from_millis(100), rx.recv()).await {
        Ok(Some(MessageContent::Data(data))) => Some(data),
        Ok(Some(msg)) => panic!("unexpected signaling message: {msg:?}"),
        Ok(None) | Err(_) => None,
    }
}

async fn wait_discovery_binding(signaler: &NostrSignaler, node_id: &NodeId, pubkey: &str) {
    timeout(Duration::from_secs(3), async {
        loop {
            if signaler
                .discovery_table
                .lock()
                .await
                .pubkey_for_node(node_id)
                .as_deref()
                == Some(pubkey)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("timed out waiting for Nostr discovery binding");
}

#[tokio::test]
async fn room_switch_clears_discovery_and_dedupe() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    signaler.set_room_id("room-a").await.unwrap();
    signaler
        .discovery_table
        .lock()
        .await
        .insert_pubkey("peer-pubkey".to_string(), u64::MAX);
    assert!(signaler.dedupe.lock().await.check_and_insert("event-id"));

    signaler.set_room_id("room-b").await.unwrap();

    assert!(signaler
        .discovery_table
        .lock()
        .await
        .active_pubkeys()
        .is_empty());
    assert!(signaler.dedupe.lock().await.check_and_insert("event-id"));
}

#[tokio::test]
async fn reset_session_rotates_identity_clears_state_and_republishes_discovery() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    let (relay_tx, mut relay_rx) = mpsc::channel(8);
    signaler.senders.lock().await.push(relay_tx);
    signaler.set_room_id("room-a").await.unwrap();
    while relay_rx.try_recv().is_ok() {}

    let old_pubkey = signaler.current_identity().await.public_key;
    signaler
        .discovery_table
        .lock()
        .await
        .insert_pubkey("peer-pubkey".to_string(), u64::MAX);
    assert!(signaler.dedupe.lock().await.check_and_insert("event-id"));

    signaler.reset_session().await.unwrap();

    let new_pubkey = signaler.current_identity().await.public_key;
    assert_ne!(old_pubkey, new_pubkey);
    assert!(signaler
        .discovery_table
        .lock()
        .await
        .active_pubkeys()
        .is_empty());
    assert!(signaler.dedupe.lock().await.check_and_insert("event-id"));

    let frame = relay_rx.recv().await.expect("reset publishes discovery");
    let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
    let event: NostrEvent = serde_json::from_value(value.as_array().unwrap()[1].clone()).unwrap();
    assert_eq!(event.pubkey, new_pubkey);
    decode_discovery_event(&signaler.codec_config, &signaler.crypto, &event, "room-a").unwrap();
}

#[tokio::test]
async fn isolated_peers_can_rejoin_with_same_node_ids_and_rotated_pubkeys() {
    let room_id = "room-a";
    let alice_id = NodeId("alice".to_string());
    let bob_id = NodeId("bob".to_string());
    let alice = NostrSignaler::new(alice_id.clone(), config());
    let bob = NostrSignaler::new(bob_id.clone(), config());
    let (alice_relay_tx, mut alice_relay_rx) = mpsc::channel(16);
    let (bob_relay_tx, mut bob_relay_rx) = mpsc::channel(16);
    alice.senders.lock().await.push(alice_relay_tx);
    bob.senders.lock().await.push(bob_relay_tx);
    alice.set_room_id(room_id).await.unwrap();
    bob.set_room_id(room_id).await.unwrap();
    while alice_relay_rx.try_recv().is_ok() {}
    while bob_relay_rx.try_recv().is_ok() {}

    let old_alice = alice.current_identity().await;
    let old_bob = bob.current_identity().await;
    let first_request = build_message_event_with_sequence(
        &bob.codec_config,
        &bob.crypto,
        &old_bob,
        &old_alice.public_key,
        &data(
            &bob_id,
            &NodeId::broadcast(),
            room_id,
            SignalingType::Request,
        ),
        1,
    )
    .unwrap();
    alice
        .process_event(first_request, mpsc::channel(4).0)
        .await
        .unwrap();
    assert_eq!(
        alice.discovery_table.lock().await.pubkey_for_node(&bob_id),
        Some(old_bob.public_key.clone())
    );

    alice.reset_session().await.unwrap();
    bob.reset_session().await.unwrap();
    while alice_relay_rx.try_recv().is_ok() {}
    while bob_relay_rx.try_recv().is_ok() {}
    let new_alice = alice.current_identity().await;
    let new_bob = bob.current_identity().await;
    assert_ne!(old_alice.public_key, new_alice.public_key);
    assert_ne!(old_bob.public_key, new_bob.public_key);

    let rejoin_request = build_message_event_with_sequence(
        &bob.codec_config,
        &bob.crypto,
        &new_bob,
        &new_alice.public_key,
        &data(
            &bob_id,
            &NodeId::broadcast(),
            room_id,
            SignalingType::Request,
        ),
        1,
    )
    .unwrap();
    alice
        .process_event(rejoin_request, mpsc::channel(4).0)
        .await
        .unwrap();

    assert_eq!(
        alice.discovery_table.lock().await.pubkey_for_node(&bob_id),
        Some(new_bob.public_key)
    );
}

#[tokio::test]
async fn requested_rejoin_request_rebinds_same_node_id_to_new_pubkey() {
    let room_id = "room-a";
    let alice_id = NodeId("alice".to_string());
    let bob_id = NodeId("bob".to_string());
    let alice = NostrSignaler::new(alice_id.clone(), config());
    let old_bob = NostrSignaler::new(bob_id.clone(), config());
    let new_bob = NostrSignaler::new(bob_id.clone(), config());
    alice.set_room_id(room_id).await.unwrap();

    let alice_identity = alice.current_identity().await;
    let old_bob_identity = old_bob.current_identity().await;
    alice
        .requested_pubkeys
        .lock()
        .await
        .insert(old_bob_identity.public_key.clone());
    let first_request = build_message_event_with_sequence(
        &old_bob.codec_config,
        &old_bob.crypto,
        &old_bob_identity,
        &alice_identity.public_key,
        &data(
            &bob_id,
            &NodeId::broadcast(),
            room_id,
            SignalingType::Request,
        ),
        1,
    )
    .unwrap();
    alice
        .process_event(first_request, mpsc::channel(4).0)
        .await
        .unwrap();
    assert_eq!(
        alice.discovery_table.lock().await.pubkey_for_node(&bob_id),
        Some(old_bob_identity.public_key.clone())
    );

    let new_bob_identity = new_bob.current_identity().await;
    alice
        .requested_pubkeys
        .lock()
        .await
        .insert(new_bob_identity.public_key.clone());
    let rejoin_request = build_message_event_with_sequence(
        &new_bob.codec_config,
        &new_bob.crypto,
        &new_bob_identity,
        &alice_identity.public_key,
        &data(
            &bob_id,
            &NodeId::broadcast(),
            room_id,
            SignalingType::Request,
        ),
        1,
    )
    .unwrap();
    alice
        .process_event(rejoin_request, mpsc::channel(4).0)
        .await
        .unwrap();

    let mut table = alice.discovery_table.lock().await;
    assert_eq!(
        table.pubkey_for_node(&bob_id),
        Some(new_bob_identity.public_key)
    );
    assert_eq!(
        table.expires_at_for_pubkey(&old_bob_identity.public_key),
        None
    );
}

#[tokio::test]
async fn active_room_republishes_discovery_before_ttl_expires() {
    let mut raw = config();
    raw.ttl_seconds = 1;
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), raw);
    let (relay_tx, mut relay_rx) = mpsc::channel(8);
    signaler.senders.lock().await.push(relay_tx);

    signaler.set_room_id("room-a").await.unwrap();

    let event = timeout(Duration::from_secs(2), async {
        loop {
            let frame = relay_rx.recv().await.expect("relay channel closed");
            let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
            let Some(items) = value.as_array() else {
                continue;
            };
            if items.first().and_then(serde_json::Value::as_str) == Some("EVENT")
                && items.len() == 2
            {
                let event: NostrEvent = serde_json::from_value(items[1].clone()).unwrap();
                if event.kind == 25049 {
                    return event;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for discovery refresh");

    decode_discovery_event(&signaler.codec_config, &signaler.crypto, &event, "room-a").unwrap();
    signaler.close().await.unwrap();
}

#[tokio::test]
async fn publish_frame_drops_dead_sender_after_partial_failure() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    let (dead_tx, dead_rx) = mpsc::channel(1);
    let (live_tx, mut live_rx) = mpsc::channel(1);
    drop(dead_rx);
    signaler.senders.lock().await.push(dead_tx);
    signaler.senders.lock().await.push(live_tx);

    signaler
        .publish_frame("relay-frame".to_string())
        .await
        .expect("one live relay sender should be enough");

    assert_eq!(
        live_rx.recv().await.as_deref(),
        Some("relay-frame"),
        "live relay should receive the frame"
    );
    assert_eq!(signaler.senders.lock().await.len(), 1);
}

#[tokio::test]
async fn publish_frame_errors_when_all_senders_are_dead() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    let (dead_a_tx, dead_a_rx) = mpsc::channel(1);
    let (dead_b_tx, dead_b_rx) = mpsc::channel(1);
    drop(dead_a_rx);
    drop(dead_b_rx);
    signaler.senders.lock().await.push(dead_a_tx);
    signaler.senders.lock().await.push(dead_b_tx);

    assert!(signaler
        .publish_frame("relay-frame".to_string())
        .await
        .is_err());
    assert!(signaler.senders.lock().await.is_empty());
}

// Regression test for the sequence-order race described on `send_order`:
// concurrent `publish_message_to_pubkey` calls to the same receiver must
// enqueue onto the relay channel in the same order their sequences were
// assigned, or the receiver's monotonic sequence gate can silently discard
// an earlier message (e.g. an Offer) that loses the race to a later one
// (e.g. a trickled ICE candidate).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_publishes_to_same_receiver_enqueue_in_ascending_sequence_order() {
    let room_id = "room-order";
    let alice = NostrSignaler::new(NodeId("alice".to_string()), config());
    let bob = NostrSignaler::new(NodeId("bob".to_string()), config());
    let (relay_tx, mut relay_rx) = mpsc::channel(256);
    alice.senders.lock().await.push(relay_tx);
    alice.set_room_id(room_id).await.unwrap();
    bob.set_room_id(room_id).await.unwrap();
    while relay_rx.try_recv().is_ok() {}

    let bob_pubkey = bob.current_identity().await.public_key;
    let alice_node = NodeId("alice".to_string());
    let bob_node = NodeId("bob".to_string());

    const CONCURRENT_PUBLISHES: usize = 20;
    let mut handles = Vec::with_capacity(CONCURRENT_PUBLISHES);
    for _ in 0..CONCURRENT_PUBLISHES {
        let alice = alice.clone();
        let bob_pubkey = bob_pubkey.clone();
        let payload = data(&alice_node, &bob_node, room_id, SignalingType::Candidate);
        handles.push(tokio::spawn(async move {
            alice.publish_message_to_pubkey(&bob_pubkey, &payload).await
        }));
    }
    for handle in handles {
        handle.await.unwrap().expect("publish should succeed");
    }

    let bob_identity = bob.current_identity().await;
    let mut sequences = Vec::with_capacity(CONCURRENT_PUBLISHES);
    while let Ok(frame) = relay_rx.try_recv() {
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        // The periodic room resubscribe (resubscribe.rs) also shares this
        // relay channel and can interleave a "REQ" frame with our "EVENT"
        // frames; only decode the message events we actually care about.
        let Some(items) = value.as_array() else {
            continue;
        };
        if items.first().and_then(serde_json::Value::as_str) != Some("EVENT") {
            continue;
        }
        let event: NostrEvent = serde_json::from_value(items[1].clone()).unwrap();
        let decoded = decode_message_event(
            &bob.codec_config,
            &bob.crypto,
            &bob_identity,
            &bob_node,
            &event,
            room_id,
        )
        .unwrap();
        sequences.push(
            decoded
                .sequence
                .expect("targeted message carries a sequence"),
        );
    }

    assert_eq!(
        sequences.len(),
        CONCURRENT_PUBLISHES,
        "every concurrent publish should have enqueued exactly one relay frame"
    );
    let mut sorted = sequences.clone();
    sorted.sort_unstable();
    assert_eq!(
        sequences, sorted,
        "relay enqueue order must match ascending sequence-assignment order"
    );
    assert_eq!(
        sorted,
        (1..=CONCURRENT_PUBLISHES as u64).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn subscribe_room_drops_dead_sender_after_partial_failure() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    let (dead_tx, dead_rx) = mpsc::channel(1);
    let (live_tx, mut live_rx) = mpsc::channel(4);
    drop(dead_rx);
    signaler.senders.lock().await.push(dead_tx);
    signaler.senders.lock().await.push(live_tx);

    signaler
        .subscribe_room("room-a")
        .await
        .expect("one live relay sender should be enough");

    let discovery_req = live_rx.recv().await.expect("discovery subscription frame");
    let message_req = live_rx.recv().await.expect("message subscription frame");
    assert!(discovery_req.contains("REQ"));
    assert!(message_req.contains("REQ"));
    assert_eq!(signaler.senders.lock().await.len(), 1);
}

#[tokio::test]
async fn subscribe_room_errors_when_all_senders_are_dead() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    let (dead_a_tx, dead_a_rx) = mpsc::channel(1);
    let (dead_b_tx, dead_b_rx) = mpsc::channel(1);
    drop(dead_a_rx);
    drop(dead_b_rx);
    signaler.senders.lock().await.push(dead_a_tx);
    signaler.senders.lock().await.push(dead_b_tx);

    assert!(signaler.subscribe_room("room-a").await.is_err());
    assert!(signaler.senders.lock().await.is_empty());
}

#[tokio::test]
async fn reconnect_resubscribes_active_room_after_relay_disconnect() {
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (frame_tx, mut frame_rx) = mpsc::channel::<(usize, String)>(16);
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_for_task = accepted.clone();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let connection_index = accepted_for_task.fetch_add(1, Ordering::SeqCst);
            let frame_tx = frame_tx.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = accept_async(stream).await else {
                    return;
                };
                if let Some(Ok(msg)) = ws.next().await {
                    if let Ok(text) = msg.into_text() {
                        let _ = frame_tx.send((connection_index, text.to_string())).await;
                    }
                }
            });
        }
    });

    let mut cfg = config();
    cfg.relays = vec![format!("ws://{addr}")];
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), cfg);
    signaler.set_room_id("room-reconnect").await.unwrap();
    let (incoming_tx, _incoming_rx) = mpsc::channel(8);

    signaler
        .connect(incoming_tx)
        .await
        .expect("connects to relay");

    timeout(Duration::from_secs(2), async {
        loop {
            let frame = frame_rx.recv().await.expect("relay frame channel closed");
            if frame.0 == 0 && frame.1.contains("REQ") {
                return;
            }
        }
    })
    .await
    .expect("initial relay connection should receive a REQ");

    let second = timeout(Duration::from_secs(3), async {
        loop {
            let frame = frame_rx.recv().await.expect("relay frame channel closed");
            if frame.0 >= 1 && frame.1.contains("REQ") {
                return frame;
            }
        }
    })
    .await
    .expect("reconnected relay should receive a REQ");
    assert!(second.1.contains("REQ"));
    assert!(accepted.load(Ordering::SeqCst) >= 2);

    signaler.close().await.unwrap();
}

/// Extracts the subscription id (second array element) from a raw
/// `["REQ", "<id>", ...]` / `["CLOSED", "<id>", ...]` relay frame.
fn extract_sub_id(frame: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(frame).expect("frame should be JSON");
    value
        .as_array()
        .and_then(|items| items.get(1))
        .and_then(serde_json::Value::as_str)
        .expect("frame should carry a subscription id")
        .to_string()
}

#[tokio::test]
async fn active_room_resubscribes_with_same_subscription_ids_before_rotation() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    let (relay_tx, mut relay_rx) = mpsc::channel(32);
    signaler.senders.lock().await.push(relay_tx);

    signaler.set_room_id("room-a").await.unwrap();

    let discovery_req = relay_rx.recv().await.expect("initial discovery REQ");
    let message_req = relay_rx.recv().await.expect("initial message REQ");
    let discovery_sub_id = extract_sub_id(&discovery_req);
    let message_sub_id = extract_sub_id(&message_req);
    assert_ne!(discovery_sub_id, message_sub_id);

    // The periodic room resubscribe should re-issue REQ frames carrying the
    // exact same subscription ids (NIP-01 filter replacement), not brand-new
    // ones, so the relay treats it as an update rather than a fresh
    // subscription.
    let (resub_discovery, resub_message) = timeout(Duration::from_secs(2), async {
        let mut seen_discovery = None;
        let mut seen_message = None;
        loop {
            let frame = relay_rx.recv().await.expect("relay channel closed");
            if !frame.contains("REQ") {
                continue;
            }
            let sub_id = extract_sub_id(&frame);
            if sub_id == discovery_sub_id {
                seen_discovery = Some(frame);
            } else if sub_id == message_sub_id {
                seen_message = Some(frame);
            }
            if let (Some(d), Some(m)) = (&seen_discovery, &seen_message) {
                return (d.clone(), m.clone());
            }
        }
    })
    .await
    .expect("timed out waiting for periodic resubscribe");

    assert_eq!(extract_sub_id(&resub_discovery), discovery_sub_id);
    assert_eq!(extract_sub_id(&resub_message), message_sub_id);

    signaler.close().await.unwrap();
}

#[tokio::test]
async fn closed_subscription_triggers_immediate_resubscribe_with_same_id() {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::protocol::Message;

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (done_tx, mut done_rx) = mpsc::channel::<bool>(1);

    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut ws) = accept_async(stream).await else {
            return;
        };

        let Some(Ok(discovery_msg)) = ws.next().await else {
            return;
        };
        let Ok(discovery_req) = discovery_msg.into_text() else {
            return;
        };
        let _ = extract_sub_id(discovery_req.as_ref());

        let Some(Ok(message_msg)) = ws.next().await else {
            return;
        };
        let Ok(message_req) = message_msg.into_text() else {
            return;
        };
        let message_sub_id = extract_sub_id(message_req.as_ref());

        // Simulate the relay unilaterally closing our message subscription
        // (e.g. rate limiting), which should trigger an immediate
        // resubscribe rather than waiting for the periodic timer.
        let closed = serde_json::json!(["CLOSED", message_sub_id, "rate-limited"]).to_string();
        if ws.send(Message::Text(closed.into())).await.is_err() {
            return;
        }

        // Look for a fresh REQ carrying the same subscription id, well
        // before the periodic resubscribe interval would fire on its own.
        let found = timeout(Duration::from_millis(150), async {
            loop {
                let Some(Ok(msg)) = ws.next().await else {
                    return false;
                };
                let Ok(text) = msg.into_text() else {
                    continue;
                };
                let text = text.to_string();
                if text.contains("REQ") && extract_sub_id(&text) == message_sub_id {
                    return true;
                }
            }
        })
        .await
        .unwrap_or(false);

        let _ = done_tx.send(found).await;
    });

    let mut cfg = config();
    cfg.relays = vec![format!("ws://{addr}")];
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), cfg);
    signaler.set_room_id("room-closed").await.unwrap();
    let (incoming_tx, _incoming_rx) = mpsc::channel(8);

    signaler
        .connect(incoming_tx)
        .await
        .expect("connects to relay");

    let resubscribed = timeout(Duration::from_secs(2), done_rx.recv())
        .await
        .expect("timed out waiting for mock relay result")
        .expect("done channel closed unexpectedly");
    assert!(
        resubscribed,
        "CLOSED subscription should trigger an immediate resubscribe with the same id"
    );

    signaler.close().await.unwrap();
}

#[tokio::test]
async fn relay_connection_sends_periodic_keepalive_pings() {
    use futures_util::StreamExt;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::protocol::Message;

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ping_tx, mut ping_rx) = mpsc::channel::<()>(8);

    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut ws) = accept_async(stream).await else {
            return;
        };
        while let Some(Ok(msg)) = ws.next().await {
            if matches!(msg, Message::Ping(_)) {
                let _ = ping_tx.send(()).await;
            }
        }
    });

    let mut cfg = config();
    cfg.relays = vec![format!("ws://{addr}")];
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), cfg);
    let (incoming_tx, _incoming_rx) = mpsc::channel(8);

    signaler
        .connect(incoming_tx)
        .await
        .expect("connects to relay");

    timeout(Duration::from_secs(2), ping_rx.recv())
        .await
        .expect("timed out waiting for a keepalive ping")
        .expect("ping channel closed unexpectedly");

    signaler.close().await.unwrap();
}

#[tokio::test]
async fn relay_connection_reconnects_after_prolonged_silence() {
    // The relay accepts the connection but never reads or writes again,
    // simulating a proxy/NAT that is holding the TCP connection open without
    // actually servicing it. Our pings keep being written successfully
    // (buffered), so only the "no inbound activity for ~2 intervals"
    // staleness check can detect this and force a reconnect.
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_for_task = accepted.clone();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            accepted_for_task.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let Ok(_ws) = accept_async(stream).await else {
                    return;
                };
                std::future::pending::<()>().await
            });
        }
    });

    let mut cfg = config();
    cfg.relays = vec![format!("ws://{addr}")];
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), cfg);
    let (incoming_tx, _incoming_rx) = mpsc::channel(8);
    signaler
        .connect(incoming_tx)
        .await
        .expect("connects to relay");
    assert_eq!(accepted.load(Ordering::SeqCst), 1);

    timeout(Duration::from_secs(3), async {
        loop {
            if accepted.load(Ordering::SeqCst) >= 2 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("signaler should reconnect after prolonged silence");

    signaler.close().await.unwrap();
}

#[tokio::test]
async fn invalid_event_does_not_block_later_valid_event_with_same_id() {
    let signaler = NostrSignaler::new(NodeId("alice".to_string()), config());
    let peer = NostrSignaler::new(NodeId("bob".to_string()), config());
    let (tx, _rx) = mpsc::channel(1);
    signaler.set_room_id("room-a").await.unwrap();
    let event =
        build_discovery_event(&peer.codec_config, &peer.crypto, &peer.identity, "room-a").unwrap();
    let mut invalid_event = event.clone();
    invalid_event.sig = "invalid".to_string();

    assert!(signaler
        .process_event(invalid_event, tx.clone())
        .await
        .is_err());
    assert!(signaler.process_event(event, tx).await.is_err());

    assert!(signaler
        .discovery_table
        .lock()
        .await
        .expires_at_for_pubkey(&peer.identity.public_key)
        .is_some());
}

mod rejoin;
mod replay;
mod security;
mod topology;

#[tokio::test]
#[ignore = "requires `just nostr-relay` or MIST_NOSTR_RELAY_URL"]
async fn live_relay_accepts_signed_discovery_event() {
    use futures_util::{SinkExt, StreamExt};
    use mistlib_core::signaling::nostr::{
        build_discovery_event, event_frame_json, parse_relay_message, RelayMessage,
    };
    use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

    let relay_url =
        std::env::var("MIST_NOSTR_RELAY_URL").unwrap_or_else(|_| "ws://127.0.0.1:7777".to_string());
    let cfg = config();
    let codec = mistlib_core::signaling::nostr::NostrCodecConfig::from_config(&cfg);
    let crypto =
        mistlib_core::signaling::nostr::InvitePskCrypto::new(&cfg.invite_salt, &cfg.invite_code);
    let identity = mistlib_core::signaling::nostr::TemporarySignalingIdentity::generate();

    let event = build_discovery_event(&codec, &crypto, &identity, "sig-test-room").unwrap();
    let frame = event_frame_json(&event).unwrap();

    let (ws_stream, _) = connect_async(&relay_url)
        .await
        .expect("should connect to relay");
    let (mut write, mut read) = ws_stream.split();

    write
        .send(Message::Text(frame.into()))
        .await
        .expect("should send event frame");

    let response = tokio::time::timeout(Duration::from_secs(5), read.next())
        .await
        .expect("relay should respond within 5s")
        .expect("relay should not close connection")
        .expect("should receive WebSocket frame");

    let raw = match response {
        Message::Text(t) => t.to_string(),
        other => panic!("unexpected frame type: {:?}", other),
    };

    eprintln!("Relay OK response: {raw}");
    let msg = parse_relay_message(&raw)
        .expect("should parse relay response")
        .expect("relay response should not be empty");

    match msg {
        RelayMessage::Ok {
            event_id,
            accepted,
            message,
        } => {
            assert!(
                accepted,
                "relay rejected event {event_id}: {message} -- signature or format issue"
            );
            eprintln!("relay accepted event {event_id}: {message}");
        }
        other => panic!("expected OK frame, got {:?}", other),
    }
}

#[tokio::test]
#[ignore = "requires `just nostr-relay` or MIST_NOSTR_RELAY_URL"]
async fn live_go_relay_exchanges_nostr_signaling() {
    let ids = LiveIds::generate();
    let alice = NostrSignaler::new(ids.alice.clone(), config());
    let bob = NostrSignaler::new(ids.bob.clone(), config());
    let (alice_tx, mut alice_rx) = mpsc::channel(16);
    let (bob_tx, mut bob_rx) = mpsc::channel(16);

    alice.connect(alice_tx).await.expect("alice connects");
    bob.connect(bob_tx).await.expect("bob connects");

    alice
        .send_signaling(
            &NodeId::server(),
            MessageContent::Data(data(
                &ids.alice,
                &NodeId::broadcast(),
                &ids.room_id,
                SignalingType::Request,
            )),
        )
        .await
        .expect("alice publishes discovery");
    bob.send_signaling(
        &NodeId::server(),
        MessageContent::Data(data(
            &ids.bob,
            &NodeId::broadcast(),
            &ids.room_id,
            SignalingType::Request,
        )),
    )
    .await
    .expect("bob publishes discovery");

    let request = recv_matching(&mut alice_rx, |data| {
        data.sender_id == ids.bob && data.signaling_type == SignalingType::Request
    })
    .await;
    assert_eq!(request.sender_id, ids.bob);
    assert_eq!(request.signaling_type, SignalingType::Request);
    let request = recv_matching(&mut bob_rx, |data| {
        data.sender_id == ids.alice && data.signaling_type == SignalingType::Request
    })
    .await;
    assert_eq!(request.sender_id, ids.alice);
    assert_eq!(request.signaling_type, SignalingType::Request);

    alice
        .send_signaling(
            &ids.bob,
            MessageContent::Data(data(
                &ids.alice,
                &ids.bob,
                &ids.room_id,
                SignalingType::Offer,
            )),
        )
        .await
        .expect("alice sends direct offer");

    let offer = recv_matching(&mut bob_rx, |data| {
        data.sender_id == ids.alice
            && data.receiver_id == ids.bob
            && data.signaling_type == SignalingType::Offer
    })
    .await;
    assert_eq!(offer.sender_id, ids.alice);
    assert_eq!(offer.receiver_id, ids.bob);
    assert_eq!(offer.signaling_type, SignalingType::Offer);
}

async fn wait_any_discovery_binding(
    signalers: &[(&NodeId, &NostrSignaler)],
    node_id: &NodeId,
    pubkey: &str,
) -> usize {
    timeout(Duration::from_secs(3), async {
        loop {
            for (idx, (_, signaler)) in signalers.iter().enumerate() {
                if signaler
                    .discovery_table
                    .lock()
                    .await
                    .pubkey_for_node(node_id)
                    .as_deref()
                    == Some(pubkey)
                {
                    return idx;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("timed out waiting for any Nostr discovery binding")
}

#[tokio::test]
#[ignore = "requires `just nostr-relay` or MIST_NOSTR_RELAY_URL"]
async fn live_go_relay_reconnects_rejoined_node_with_same_id() {
    let ids = LiveIds::generate();
    let alice = NostrSignaler::new(ids.alice.clone(), config());
    let bob = NostrSignaler::new(ids.bob.clone(), config());
    let (alice_tx, mut alice_rx) = mpsc::channel(32);
    let (bob_tx, mut bob_rx) = mpsc::channel(32);

    alice.connect(alice_tx).await.expect("alice connects");
    bob.connect(bob_tx).await.expect("bob connects");

    alice
        .send_signaling(
            &NodeId::server(),
            MessageContent::Data(data(
                &ids.alice,
                &NodeId::broadcast(),
                &ids.room_id,
                SignalingType::Request,
            )),
        )
        .await
        .expect("alice publishes discovery");
    bob.send_signaling(
        &NodeId::server(),
        MessageContent::Data(data(
            &ids.bob,
            &NodeId::broadcast(),
            &ids.room_id,
            SignalingType::Request,
        )),
    )
    .await
    .expect("bob publishes discovery");

    let initial_bob_pubkey = bob.current_identity().await.public_key;
    wait_discovery_binding(&alice, &ids.bob, &initial_bob_pubkey).await;

    alice
        .send_signaling(
            &ids.bob,
            MessageContent::Data(data(
                &ids.alice,
                &ids.bob,
                &ids.room_id,
                SignalingType::Offer,
            )),
        )
        .await
        .expect("alice sends initial direct offer");
    let initial_offer = recv_matching(&mut bob_rx, |data| {
        data.sender_id == ids.alice
            && data.receiver_id == ids.bob
            && data.signaling_type == SignalingType::Offer
    })
    .await;
    assert_eq!(initial_offer.sender_id, ids.alice);

    bob.close().await.expect("bob leaves");
    while recv_available(&mut alice_rx).await.is_some() {}
    while recv_available(&mut bob_rx).await.is_some() {}

    let rejoined_bob = NostrSignaler::new(ids.bob.clone(), config());
    let (rejoined_bob_tx, mut rejoined_bob_rx) = mpsc::channel(32);
    rejoined_bob
        .connect(rejoined_bob_tx)
        .await
        .expect("bob rejoins");
    rejoined_bob
        .send_signaling(
            &NodeId::server(),
            MessageContent::Data(data(
                &ids.bob,
                &NodeId::broadcast(),
                &ids.room_id,
                SignalingType::Request,
            )),
        )
        .await
        .expect("rejoined bob publishes discovery");

    let rejoined_bob_pubkey = rejoined_bob.current_identity().await.public_key;
    assert_ne!(initial_bob_pubkey, rejoined_bob_pubkey);
    wait_discovery_binding(&alice, &ids.bob, &rejoined_bob_pubkey).await;

    alice
        .send_signaling(
            &ids.bob,
            MessageContent::Data(data(
                &ids.alice,
                &ids.bob,
                &ids.room_id,
                SignalingType::Offer,
            )),
        )
        .await
        .expect("alice sends offer after bob rejoins");

    let rejoin_offer = recv_matching(&mut rejoined_bob_rx, |data| {
        data.sender_id == ids.alice
            && data.receiver_id == ids.bob
            && data.signaling_type == SignalingType::Offer
    })
    .await;
    assert_eq!(rejoin_offer.sender_id, ids.alice);
    assert_eq!(rejoin_offer.receiver_id, ids.bob);
    assert_eq!(rejoin_offer.signaling_type, SignalingType::Offer);

    alice.close().await.unwrap();
    rejoined_bob.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires `just nostr-relay` or MIST_NOSTR_RELAY_URL"]
async fn live_go_relay_reconnects_rejoined_node_with_same_id_among_four_nodes() {
    let run_id = random_subscription_id();
    let room_id = format!("nostr-live-four-rejoin-room-{run_id}");
    let alice_id = NodeId(format!("alice-{run_id}"));
    let bob_id = NodeId(format!("bob-{run_id}"));
    let carol_id = NodeId(format!("carol-{run_id}"));
    let dave_id = NodeId(format!("dave-{run_id}"));

    let alice = NostrSignaler::new(alice_id.clone(), config());
    let bob = NostrSignaler::new(bob_id.clone(), config());
    let carol = NostrSignaler::new(carol_id.clone(), config());
    let dave = NostrSignaler::new(dave_id.clone(), config());
    let (alice_tx, mut alice_rx) = mpsc::channel(32);
    let (bob_tx, mut bob_rx) = mpsc::channel(32);
    let (carol_tx, mut carol_rx) = mpsc::channel(32);
    let (dave_tx, mut dave_rx) = mpsc::channel(32);

    alice.connect(alice_tx).await.expect("alice connects");
    bob.connect(bob_tx).await.expect("bob connects");
    carol.connect(carol_tx).await.expect("carol connects");
    dave.connect(dave_tx).await.expect("dave connects");

    let stable_nodes = [(&alice_id, &alice), (&carol_id, &carol), (&dave_id, &dave)];

    for (id, signaler) in [
        (&alice_id, &alice),
        (&bob_id, &bob),
        (&carol_id, &carol),
        (&dave_id, &dave),
    ] {
        signaler
            .send_signaling(
                &NodeId::server(),
                MessageContent::Data(data(
                    id,
                    &NodeId::broadcast(),
                    &room_id,
                    SignalingType::Request,
                )),
            )
            .await
            .expect("node publishes discovery");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let initial_bob_pubkey = bob.current_identity().await.public_key;
    let initial_sender_idx =
        wait_any_discovery_binding(&stable_nodes, &bob_id, &initial_bob_pubkey).await;
    let (initial_sender_id, initial_sender) = stable_nodes[initial_sender_idx];

    initial_sender
        .send_signaling(
            &bob_id,
            MessageContent::Data(data(
                initial_sender_id,
                &bob_id,
                &room_id,
                SignalingType::Offer,
            )),
        )
        .await
        .expect("stable node sends initial offer to bob");
    let initial_offer = recv_matching(&mut bob_rx, |data| {
        data.sender_id == *initial_sender_id
            && data.receiver_id == bob_id
            && data.signaling_type == SignalingType::Offer
    })
    .await;
    assert_eq!(initial_offer.receiver_id, bob_id);

    bob.close().await.expect("bob leaves");
    while recv_available(&mut alice_rx).await.is_some() {}
    while recv_available(&mut bob_rx).await.is_some() {}
    while recv_available(&mut carol_rx).await.is_some() {}
    while recv_available(&mut dave_rx).await.is_some() {}

    let rejoined_bob = NostrSignaler::new(bob_id.clone(), config());
    let (rejoined_bob_tx, mut rejoined_bob_rx) = mpsc::channel(32);
    rejoined_bob
        .connect(rejoined_bob_tx)
        .await
        .expect("bob rejoins");
    rejoined_bob
        .send_signaling(
            &NodeId::server(),
            MessageContent::Data(data(
                &bob_id,
                &NodeId::broadcast(),
                &room_id,
                SignalingType::Request,
            )),
        )
        .await
        .expect("rejoined bob publishes discovery");

    let rejoined_bob_pubkey = rejoined_bob.current_identity().await.public_key;
    assert_ne!(initial_bob_pubkey, rejoined_bob_pubkey);
    let rejoin_sender_idx =
        wait_any_discovery_binding(&stable_nodes, &bob_id, &rejoined_bob_pubkey).await;
    let (rejoin_sender_id, rejoin_sender) = stable_nodes[rejoin_sender_idx];

    rejoin_sender
        .send_signaling(
            &bob_id,
            MessageContent::Data(data(
                rejoin_sender_id,
                &bob_id,
                &room_id,
                SignalingType::Offer,
            )),
        )
        .await
        .expect("stable node sends offer after bob rejoins");
    let rejoin_offer = recv_matching(&mut rejoined_bob_rx, |data| {
        data.sender_id == *rejoin_sender_id
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
