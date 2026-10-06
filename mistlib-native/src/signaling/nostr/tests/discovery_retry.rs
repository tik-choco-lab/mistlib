use super::*;
use mistlib_core::signaling::SignalingHandler;
use mistlib_core::transport::Transport;
use mistlib_core::types::ConnectionState;
use std::sync::Arc;

#[tokio::test]
async fn queued_ordinary_is_promoted_by_explicit_request() {
    let (mut local, mut wire) = mock_signaler("local").await;
    local.request_jitter = Duration::from_millis(900);
    for i in 0..3 {
        let peer = NostrSignaler::new(NodeId(format!("ordinary-{i}")), config());
        local
            .send_request_to_pubkey(&peer.identity.public_key, ROOM)
            .await
            .unwrap();
    }
    let live = NostrSignaler::new(NodeId("live".into()), config());
    local
        .send_request_to_pubkey(&live.identity.public_key, ROOM)
        .await
        .unwrap();
    let (tx, _rx) = mpsc::channel(4);
    local
        .process_event(request(&live, &local, 1), tx)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let live_sent = local
        .outgoing_sequences
        .lock()
        .await
        .contains_key(&live.identity.public_key);
    while wire.try_recv().is_ok() {}
    local.close().await.unwrap();
    assert!(
        live_sent,
        "explicit live reply stayed behind ordinary backlog"
    );
}

#[tokio::test]
async fn close_cancels_pacing_before_waiting_for_sender_lock() {
    let local = NostrSignaler::new(NodeId("local".into()), config());
    *local.room_id.lock().await = Some(ROOM.into());
    let (tx, rx) = mpsc::channel(1);
    tx.send("already full".into()).await.unwrap();
    local.senders.lock().await.push(tx);
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    local
        .publish_request_to_pubkey(&peer.identity.public_key, ROOM)
        .await
        .unwrap();
    timeout(Duration::from_secs(1), async {
        while local.senders.try_lock().is_ok() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let closed = timeout(Duration::from_millis(200), local.close()).await;
    local.pacing_cancel.lock().await.cancel();
    drop(rx);
    local.close().await.unwrap();
    assert!(
        closed.is_ok(),
        "close waited on a pacing worker it had not cancelled"
    );
}

const ROOM: &str = "discovery-retry-test";

#[tokio::test]
async fn repeated_request_does_not_duplicate_an_inflight_reply() {
    let (local, mut wire) = mock_signaler("local").await;
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    let order = local.send_order.lock().await;
    local
        .publish_request_to_pubkey(&peer.identity.public_key, ROOM)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(25)).await;
    let (tx, _rx) = mpsc::channel(4);
    local
        .process_event(request(&peer, &local, 1), tx)
        .await
        .unwrap();
    drop(order);
    next_event(&mut wire).await;
    let duplicate = matches!(
        timeout(Duration::from_millis(250), wire.recv()).await,
        Ok(Some(_))
    );
    local.close().await.unwrap();
    assert!(
        !duplicate,
        "inflight reply was duplicated without a retry reservation"
    );
}

#[tokio::test]
async fn discovery_observability_reports_sends_stops_and_overflow() {
    // Other library tests initialize the application logger. Finish that once
    // before installing the capture so it cannot change callsite interest
    // concurrently with this test's scoped subscriber.
    std::sync::LazyLock::force(&crate::app::INIT_LOG);
    #[derive(Clone)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = Capture(bytes.clone());
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    let (local, mut wire) = mock_signaler("local").await;
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    let key = &peer.identity.public_key;
    local.publish_request_to_pubkey(key, ROOM).await.unwrap();
    next_event(&mut wire).await;
    local
        .exchanges
        .lock()
        .await
        .request(key, web_time::Instant::now() + Duration::from_secs(100));
    local
        .queue_request(
            key,
            ROOM,
            local.session_epoch(),
            mistlib_core::signaling::nostr::DiscoveryPriority::Probe,
            Duration::ZERO,
        )
        .await;
    next_event(&mut wire).await;
    local
        .exchanges
        .lock()
        .await
        .progress(key, web_time::Instant::now());
    local.maintain_bootstrap(2).await;
    // Reserve a full capped budget without waiting for backoff clocks.
    let mut capped = mistlib_core::signaling::nostr::DiscoveryExchanges::default();
    let now = web_time::Instant::now();
    // Exercise a known pending -> progressed transition independently of the
    // asynchronously published exchange, which may already have progressed.
    let mut progressing = mistlib_core::signaling::nostr::DiscoveryExchanges::default();
    assert!(progressing.request(key, now));
    assert!(progressing.pending(key));
    progressing.progress(key, now);
    assert!(!progressing.pending(key));
    capped.request(key, now);
    for i in 1..=4 {
        capped.request(key, now + Duration::from_secs(i * 100));
    }
    for i in 0..33 {
        let candidate = NostrSignaler::new(NodeId(format!("overflow-{i}")), config());
        local
            .send_request_to_pubkey(&candidate.identity.public_key, ROOM)
            .await
            .unwrap();
    }
    // Check the room-switch log before close can supply the same stop reason.
    let before_switch = bytes.lock().unwrap().len();
    local.set_room_id("next-room").await.unwrap();
    let room_log = String::from_utf8(bytes.lock().unwrap()[before_switch..].to_vec()).unwrap();
    assert!(
        room_log.contains("INFO") && room_log.contains("reason=session change"),
        "missing room-switch stop: {room_log}"
    );
    local.close().await.unwrap();
    let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    for required in [
        "INFO",
        "discovery retry sent",
        "attempt=1/3",
        "session=1/12",
        "reason=progressed",
        "reason=target reached",
        "reason=cap",
        "reason=session change",
        "pacing overflow drop",
        "DEBUG",
        "paced discovery sent",
    ] {
        assert!(log.contains(required), "missing {required}: {log}");
    }
    assert!(log.contains(&key[..8]));
    assert!(!log.contains(key), "logs must contain pubkey prefixes only");
    assert!(log.is_ascii());
}

#[tokio::test]
async fn inflight_retry_stops_when_peer_is_confirmed_alive() {
    let (local, mut wire) = mock_signaler("zzz").await;
    let peer = NostrSignaler::new(NodeId("aaa".into()), config());
    let (tx, _rx) = mpsc::channel(4);
    local
        .process_event(request(&peer, &local, 1), tx)
        .await
        .unwrap();
    next_event(&mut wire).await;
    tokio::time::sleep(Duration::from_millis(1550)).await;
    let order = local.send_order.lock().await;
    local.maintain_bootstrap(0).await;
    tokio::time::sleep(Duration::from_millis(25)).await;
    local.note_peer_alive(&peer.local_node_id).await;
    drop(order);
    let emitted = timeout(Duration::from_millis(200), wire.recv())
        .await
        .is_ok();
    local.close().await.unwrap();
    assert!(
        !emitted,
        "retry already waiting on send_order escaped confirmed progress"
    );
}

#[tokio::test]
async fn backpressured_retry_stops_when_peer_is_confirmed_alive() {
    let (local, mut wire) = mock_signaler("zzz").await;
    let peer = NostrSignaler::new(NodeId("aaa".into()), config());
    let (incoming, _rx) = mpsc::channel(4);
    local
        .process_event(request(&peer, &local, 1), incoming)
        .await
        .unwrap();
    next_event(&mut wire).await;
    let (tx, mut blocked_wire) = mpsc::channel(1);
    tx.send("full".into()).await.unwrap();
    *local.senders.lock().await = vec![tx];
    assert!(local.exchanges.lock().await.request(
        &peer.identity.public_key,
        web_time::Instant::now() + Duration::from_secs(100)
    ));
    local
        .queue_request(
            &peer.identity.public_key,
            ROOM,
            local.session_epoch(),
            mistlib_core::signaling::nostr::DiscoveryPriority::Probe,
            Duration::ZERO,
        )
        .await;
    timeout(Duration::from_secs(1), async {
        while local.senders.try_lock().is_ok() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    local.note_peer_alive(&peer.local_node_id).await;
    assert_eq!(blocked_wire.recv().await.unwrap(), "full");
    let emitted = matches!(
        timeout(Duration::from_millis(200), blocked_wire.recv()).await,
        Ok(Some(_))
    );
    local.close().await.unwrap();
    assert!(
        !emitted,
        "retry escaped progress while waiting for relay queue space"
    );
}

#[tokio::test]
async fn routed_request_does_not_cancel_its_identifying_reply() {
    routed_request_preserves_reply("zzz", "aaa").await;
}

#[tokio::test]
async fn routed_request_preserves_reply_with_lower_local_id() {
    routed_request_preserves_reply("aaa", "zzz").await;
}

async fn routed_request_preserves_reply(local_id: &str, peer_id: &str) {
    use mistlib_core::overlay::{ActionHandler, OverlayRouter, OverlayTransport};
    use mistlib_core::signaling::{RoutedSignaler, RoutedSignalingHandler, SignalingRoute};
    struct Noop;
    impl ActionHandler for Noop {
        fn handle_action(&self, _: mistlib_core::action::OverlayAction) {}
    }
    let (local, mut wire) = mock_signaler(local_id).await;
    let local = Arc::new(local);
    let peer = NostrSignaler::new(NodeId(peer_id.into()), config());
    let router = Arc::new(OverlayRouter::new(
        &mistlib_core::config::Config::new_default(),
        Arc::new(std::sync::Mutex::new(
            mistlib_core::overlay::node_store::NodeStore::new(),
        )),
        local.local_node_id.clone(),
    ));
    let overlay = Arc::new(OverlayTransport {
        router,
        action_handler: Arc::new(Noop),
    });
    let routes = Arc::new(RoutedSignaler::new(local.clone(), overlay));
    let transport = Arc::new(crate::transports::webrtc::WebRtcTransport::new(
        routes.clone(),
        local.local_node_id.clone(),
    ));
    transport.set_room_id(ROOM.into());
    let handler = RoutedSignalingHandler::new(routes, transport.clone(), SignalingRoute::WebSocket);
    let (tx, mut rx) = mpsc::channel(4);
    local
        .process_event(request(&peer, &local, 1), tx)
        .await
        .unwrap();
    handler
        .handle_message(rx.recv().await.unwrap())
        .await
        .unwrap();
    let pending = local
        .exchanges
        .lock()
        .await
        .pending(&peer.identity.public_key);
    let replied = timeout(Duration::from_millis(300), async {
        while let Some(frame) = wire.recv().await {
            let decoded = decode_message_event(
                &local.codec_config,
                &local.crypto,
                &peer.identity,
                &peer.local_node_id,
                &event(&frame),
                ROOM,
            )
            .unwrap();
            if decoded.data.signaling_type == SignalingType::Request {
                return;
            }
        }
        panic!("wire closed before identifying reply");
    })
    .await
    .is_ok();
    transport.close_all_peer_connections().await;
    local.close().await.unwrap();
    assert!(
        pending,
        "production routing completed discovery on Request alone"
    );
    assert!(
        replied,
        "production routed Request marked exchange progressed before its initial identifying reply"
    );
}

async fn mock_signaler(id: &str) -> (NostrSignaler, mpsc::Receiver<String>) {
    let signaler = NostrSignaler::new(NodeId(id.into()), config());
    *signaler.room_id.lock().await = Some(ROOM.into());
    let (tx, rx) = mpsc::channel(128);
    signaler.senders.lock().await.push(tx);
    (signaler, rx)
}

fn event(frame: &str) -> NostrEvent {
    let value: serde_json::Value = serde_json::from_str(frame).unwrap();
    assert_eq!(value[0], "EVENT");
    serde_json::from_value(value[1].clone()).unwrap()
}

async fn next_event(rx: &mut mpsc::Receiver<String>) -> NostrEvent {
    event(
        &timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap(),
    )
}

fn request(sender: &NostrSignaler, receiver: &NostrSignaler, sequence: u64) -> NostrEvent {
    build_message_event_with_sequence(
        &sender.codec_config,
        &sender.crypto,
        &sender.identity,
        &receiver.identity.public_key,
        &data(
            &sender.local_node_id,
            &NodeId::broadcast(),
            ROOM,
            SignalingType::Request,
        ),
        sequence,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_first_identifying_reply_recovers_and_connects_in_both_node_orders() {
    for (requester_id, provider_id) in [("aaa", "zzz"), ("zzz", "aaa")] {
        let (requester, mut requester_wire) = mock_signaler(requester_id).await;
        let (provider, mut provider_wire) = mock_signaler(provider_id).await;
        let (requester_tx, mut requester_rx) = mpsc::channel(128);
        let (provider_tx, mut provider_rx) = mpsc::channel(128);
        requester
            .requested_pubkeys
            .lock()
            .await
            .insert(provider.identity.public_key.clone());
        requester
            .publish_request_to_pubkey(&provider.identity.public_key, ROOM)
            .await
            .unwrap();
        provider
            .process_event(next_event(&mut requester_wire).await, provider_tx.clone())
            .await
            .unwrap();
        let dropped = next_event(&mut provider_wire).await;
        assert_eq!(dropped.pubkey, provider.identity.public_key);
        // The mock relay discards the first identifying reply after local enqueue.
        assert!(requester
            .discovery_table
            .lock()
            .await
            .pubkey_for_node(&provider.local_node_id)
            .is_none());

        tokio::time::sleep(Duration::from_millis(1550)).await;
        requester.maintain_bootstrap(0).await;
        provider
            .process_event(next_event(&mut requester_wire).await, provider_tx.clone())
            .await
            .unwrap();
        requester
            .process_event(next_event(&mut provider_wire).await, requester_tx.clone())
            .await
            .unwrap();
        assert_eq!(
            requester
                .discovery_table
                .lock()
                .await
                .pubkey_for_node(&provider.local_node_id),
            Some(provider.identity.public_key.clone()),
        );

        // Exercise real SDP/ICE and native peer connections through the same
        // mock relay, without identity rotation or isolation recovery.
        let requester = Arc::new(requester);
        let provider = Arc::new(provider);
        let a = Arc::new(crate::transports::webrtc::WebRtcTransport::new(
            requester.clone(),
            requester.local_node_id.clone(),
        ));
        let b = Arc::new(crate::transports::webrtc::WebRtcTransport::new(
            provider.clone(),
            provider.local_node_id.clone(),
        ));
        a.set_room_id(ROOM.into());
        b.set_room_id(ROOM.into());
        let target = provider.clone();
        let wire_b = tokio::spawn(async move {
            while let Some(frame) = requester_wire.recv().await {
                target
                    .process_event(event(&frame), provider_tx.clone())
                    .await
                    .unwrap();
            }
        });
        let target = requester.clone();
        let wire_a = tokio::spawn(async move {
            while let Some(frame) = provider_wire.recv().await {
                target
                    .process_event(event(&frame), requester_tx.clone())
                    .await
                    .unwrap();
            }
        });
        let target = a.clone();
        let dispatch_a = tokio::spawn(async move {
            while let Some(msg) = requester_rx.recv().await {
                target.handle_message(msg).await.unwrap();
            }
        });
        let target = b.clone();
        let dispatch_b = tokio::spawn(async move {
            while let Some(msg) = provider_rx.recv().await {
                target.handle_message(msg).await.unwrap();
            }
        });
        timeout(Duration::from_secs(10), async {
            while a.get_connection_state(&provider.local_node_id) != ConnectionState::Connected
                || b.get_connection_state(&requester.local_node_id) != ConnectionState::Connected
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("both peers connect after the bounded discovery retry");
        assert_eq!(
            requester.current_identity().await.public_key,
            requester.identity.public_key
        );
        assert_eq!(
            provider.current_identity().await.public_key,
            provider.identity.public_key
        );
        assert!(requester
            .exchanges
            .lock()
            .await
            .poll(web_time::Instant::now() + Duration::from_secs(100))
            .is_empty());
        assert!(provider
            .exchanges
            .lock()
            .await
            .poll(web_time::Instant::now() + Duration::from_secs(100))
            .is_empty());
        // Abort routing before closing PCs: cleanup may emit repair signaling.
        for task in [wire_a, wire_b, dispatch_a, dispatch_b] {
            task.abort();
        }
        a.close_all_peer_connections().await;
        b.close_all_peer_connections().await;
        requester.close().await.unwrap();
        provider.close().await.unwrap();
    }
}

#[tokio::test]
async fn unanswered_request_retries_three_times_then_stops() {
    let (local, mut wire) = mock_signaler("local").await;
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    local
        .publish_request_to_pubkey(&peer.identity.public_key, ROOM)
        .await
        .unwrap();
    let first = next_event(&mut wire).await;
    for delay in [1550, 3050, 6050] {
        local.maintain_bootstrap(0).await;
        assert!(wire.try_recv().is_err(), "not yet due");
        tokio::time::sleep(Duration::from_millis(delay)).await;
        local.maintain_bootstrap(0).await;
        assert_ne!(
            next_event(&mut wire).await.id,
            first.id,
            "fresh event/message id"
        );
    }
    tokio::time::sleep(Duration::from_millis(1550)).await;
    local.maintain_bootstrap(0).await;
    assert!(
        wire.try_recv().is_err(),
        "three retries is a hard session cap"
    );
}

#[tokio::test]
async fn bound_responder_replies_again_with_backoff_and_cap() {
    let (local, mut wire) = mock_signaler("local").await;
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    let (tx, _rx) = mpsc::channel(32);
    let mut sequence = 0;
    for delay in [0, 1550, 3050, 6050] {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        sequence += 1;
        local
            .process_event(request(&peer, &local, sequence), tx.clone())
            .await
            .unwrap();
        next_event(&mut wire).await;
        // Fresh valid repeated Requests cannot bypass the backoff.
        for _ in 0..4 {
            sequence += 1;
            local
                .process_event(request(&peer, &local, sequence), tx.clone())
                .await
                .unwrap();
        }
        assert!(wire.try_recv().is_err());
    }
    tokio::time::sleep(Duration::from_millis(1550)).await;
    sequence += 1;
    local
        .process_event(request(&peer, &local, sequence), tx)
        .await
        .unwrap();
    assert!(
        wire.try_recv().is_err(),
        "bound peer cannot restart its exhausted budget"
    );
}

#[tokio::test]
async fn connected_peer_and_session_changes_stop_pending_retries() {
    for reset in [false, true] {
        let (local, mut wire) = mock_signaler("local").await;
        let peer = NostrSignaler::new(NodeId("peer".into()), config());
        let (tx, _rx) = mpsc::channel(4);
        local
            .process_event(request(&peer, &local, 1), tx)
            .await
            .unwrap();
        next_event(&mut wire).await;
        if reset {
            local.reset_session().await.unwrap();
            while wire.try_recv().is_ok() {}
        } else {
            local.note_peer_alive(&peer.local_node_id).await;
        }
        tokio::time::sleep(Duration::from_millis(1550)).await;
        local.maintain_bootstrap(0).await;
        while let Ok(frame) = wire.try_recv() {
            let published = event(&frame);
            assert_ne!(published.kind, local.codec_config.message_kind);
        }
        local.close().await.unwrap();
    }
}

#[tokio::test]
async fn jittered_discovery_reply_does_not_block_queued_offer_on_relay_reader() {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let mut cfg = config();
    cfg.relays = vec![format!("ws://{}", listener.local_addr().unwrap())];
    let mut local = NostrSignaler::new(NodeId("local".into()), cfg);
    local.request_jitter = Duration::from_millis(600);
    *local.room_id.lock().await = Some(ROOM.into());
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    let discovery =
        build_discovery_event(&peer.codec_config, &peer.crypto, &peer.identity, ROOM).unwrap();
    let offer = build_message_event_with_sequence(
        &peer.codec_config,
        &peer.crypto,
        &peer.identity,
        &local.identity.public_key,
        &data(
            &peer.local_node_id,
            &local.local_node_id,
            ROOM,
            SignalingType::Offer,
        ),
        1,
    )
    .unwrap();
    let relay = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        for event in [discovery, offer] {
            ws.send(Message::Text(
                serde_json::json!(["EVENT", "test", event])
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        }
        while ws.next().await.is_some() {}
    });
    let (tx, mut rx) = mpsc::channel(4);
    local.connect(tx).await.unwrap();
    let received = timeout(Duration::from_millis(250), rx.recv())
        .await
        .expect("Offer behind discovery must precede the 600ms jitter")
        .unwrap();
    assert!(matches!(
        received,
        MessageContent::Data(SignalingData {
            signaling_type: SignalingType::Offer,
            ..
        })
    ));
    local.close().await.unwrap();
    relay.abort();
}

#[tokio::test]
async fn delayed_reply_is_cancelled_after_room_switch_or_identity_reset() {
    for reset in [false, true] {
        let (mut local, mut wire) = mock_signaler("local").await;
        local.request_jitter = Duration::from_millis(200);
        let peer = NostrSignaler::new(NodeId("peer".into()), config());
        let discovery =
            build_discovery_event(&peer.codec_config, &peer.crypto, &peer.identity, ROOM).unwrap();
        local
            .process_event(discovery, mpsc::channel(4).0)
            .await
            .unwrap();
        assert!(
            wire.try_recv().is_err(),
            "reply is scheduled, not sent inline"
        );
        if reset {
            local.reset_session().await.unwrap();
        } else {
            local.set_room_id("different-room").await.unwrap();
        }
        while wire.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(250)).await;
        while let Ok(frame) = wire.try_recv() {
            assert_ne!(
                event(&frame).kind,
                local.codec_config.message_kind,
                "stale delayed Request escaped"
            );
        }
        assert!(local.outbox.lock().await.worker_epoch.is_none());
        local.close().await.unwrap();
    }
}

#[tokio::test]
async fn delayed_reply_waiting_for_publish_lock_cannot_cross_identity_reset() {
    let (mut local, mut wire) = mock_signaler("local").await;
    local.request_jitter = Duration::from_millis(50);
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    let held_order = local.send_order.lock().await;
    local
        .send_request_to_pubkey(&peer.identity.public_key, ROOM)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        local.outbox.lock().await.worker_epoch.is_some(),
        "pacing worker is waiting on the publish lock"
    );
    local.reset_session().await.unwrap();
    while wire.try_recv().is_ok() {}
    drop(held_order);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        wire.try_recv().is_err(),
        "old delayed task must not send with the rotated identity"
    );
    assert!(local.outbox.lock().await.worker_epoch.is_none());
    local.close().await.unwrap();
}

#[tokio::test]
async fn two_connected_peers_stop_dead_identity_retries() {
    let (local, mut wire) = mock_signaler("local").await;
    let peer = NostrSignaler::new(NodeId("dead".into()), config());
    local
        .publish_request_to_pubkey(&peer.identity.public_key, ROOM)
        .await
        .unwrap();
    next_event(&mut wire).await;
    tokio::time::sleep(Duration::from_millis(1550)).await;
    local.maintain_bootstrap(2).await;
    assert!(
        wire.try_recv().is_err(),
        "dead identity retried after target connectivity"
    );
}

#[tokio::test]
async fn lost_outgoing_offer_does_not_cancel_identifying_reply() {
    let (local, mut wire) = mock_signaler("aaa").await;
    let peer = NostrSignaler::new(NodeId("zzz".into()), config());
    let (tx, _rx) = mpsc::channel(8);
    local
        .process_event(request(&peer, &local, 1), tx.clone())
        .await
        .unwrap();
    next_event(&mut wire).await; // Relay discards the identifying reply.
    local
        .publish_message_to_pubkey(
            &peer.identity.public_key,
            &data(
                &local.local_node_id,
                &peer.local_node_id,
                ROOM,
                SignalingType::Offer,
            ),
        )
        .await
        .unwrap();
    next_event(&mut wire).await; // Relay discards outgoing SDP as well.
    tokio::time::sleep(Duration::from_millis(1550)).await;
    local
        .process_event(request(&peer, &local, 2), tx)
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_secs(1), wire.recv())
            .await
            .unwrap()
            .is_some(),
        "an unacknowledged outgoing Offer permanently suppressed identifying replies"
    );
}

#[tokio::test]
async fn historical_backlog_does_not_bypass_all_request_jitter() {
    let (mut local, mut wire) = mock_signaler("local").await;
    local.request_jitter = Duration::from_millis(900);
    // Find a low-ranked local identity so collecting forward identities is cheap.
    loop {
        let rank = local
            .codec_config
            .topology_rank(ROOM, &local.identity.public_key);
        if rank.starts_with('0') {
            break;
        }
        local = mock_signaler("local").await.0;
        let (sender, receiver) = mpsc::channel(128);
        local.senders.lock().await.clear();
        local.senders.lock().await.push(sender);
        wire = receiver;
        local.request_jitter = Duration::from_millis(900);
    }
    let local_rank = local
        .codec_config
        .topology_rank(ROOM, &local.identity.public_key);
    let mut identities = Vec::new();
    while identities.len() < 40 {
        let identity = mistlib_core::signaling::nostr::TemporarySignalingIdentity::generate();
        let rank = local.codec_config.topology_rank(ROOM, &identity.public_key);
        if rank > local_rank {
            identities.push((rank, identity));
        }
    }
    identities.sort_by(|a, b| b.0.cmp(&a.0));
    let events: Vec<_> = identities
        .into_iter()
        .map(|(_, identity)| {
            build_discovery_event(&local.codec_config, &local.crypto, &identity, ROOM).unwrap()
        })
        .collect();
    let started = std::time::Instant::now();
    let (tx, _rx) = mpsc::channel(1);
    for event in events {
        local.process_event(event, tx.clone()).await.unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "fixture too slow"
    );
    let mut immediate = 0;
    while wire.try_recv().is_ok() {
        immediate += 1;
    }
    assert_eq!(
        immediate, 0,
        "historical backlog emitted {immediate} immediate publishes before any jitter elapsed"
    );
}

#[tokio::test]
async fn overflow_releases_reservations_and_explicit_replies_share_pacing() {
    let (mut local, mut wire) = mock_signaler("local").await;
    local.request_jitter = Duration::from_millis(900);
    let first = NostrSignaler::new(NodeId("first".into()), config());
    local
        .send_request_to_pubkey(&first.identity.public_key, ROOM)
        .await
        .unwrap();
    for _ in 0..39 {
        let peer = NostrSignaler::new(NodeId("peer".into()), config());
        local
            .send_request_to_pubkey(&peer.identity.public_key, ROOM)
            .await
            .unwrap();
    }
    assert!(!local
        .exchanges
        .lock()
        .await
        .pending(&first.identity.public_key));
    let second = NostrSignaler::new(NodeId("second".into()), config());
    let (tx, _rx) = mpsc::channel(4);
    local
        .process_event(request(&first, &local, 1), tx.clone())
        .await
        .unwrap();
    local
        .process_event(request(&second, &local, 1), tx)
        .await
        .unwrap();
    let first_reply = next_event(&mut wire).await;
    assert_eq!(first_reply.pubkey, local.identity.public_key);
    assert_eq!(
        local
            .outgoing_sequences
            .lock()
            .await
            .get(&first.identity.public_key),
        Some(&1)
    );
    assert!(!local
        .outgoing_sequences
        .lock()
        .await
        .contains_key(&second.identity.public_key));
    let sent = std::time::Instant::now();
    let second_reply = next_event(&mut wire).await;
    assert!(sent.elapsed() >= Duration::from_millis(90));
    assert_eq!(second_reply.pubkey, local.identity.public_key);
    assert_eq!(
        local
            .outgoing_sequences
            .lock()
            .await
            .get(&second.identity.public_key),
        Some(&1)
    );
    local.close().await.unwrap();
}

#[tokio::test]
async fn queued_retry_waiting_for_send_order_stops_at_target() {
    let (local, mut wire) = mock_signaler("local").await;
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    local
        .publish_request_to_pubkey(&peer.identity.public_key, ROOM)
        .await
        .unwrap();
    next_event(&mut wire).await;
    tokio::time::sleep(Duration::from_millis(1550)).await;
    let order = local.send_order.lock().await;
    local.maintain_bootstrap(0).await;
    tokio::time::sleep(Duration::from_millis(25)).await;
    local.maintain_bootstrap(2).await;
    drop(order);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(wire.try_recv().is_err());
    local.close().await.unwrap();
}

#[tokio::test]
async fn confirmed_progress_releases_blocked_worker() {
    let (local, mut wire) = mock_signaler("zzz").await;
    let peer = NostrSignaler::new(NodeId("aaa".into()), config());
    let (incoming, _rx) = mpsc::channel(4);
    local
        .process_event(request(&peer, &local, 1), incoming)
        .await
        .unwrap();
    next_event(&mut wire).await;
    let (tx, blocked_wire) = mpsc::channel(1);
    tx.send("full".into()).await.unwrap();
    *local.senders.lock().await = vec![tx];
    assert!(local.exchanges.lock().await.request(
        &peer.identity.public_key,
        web_time::Instant::now() + Duration::from_secs(100)
    ));
    local
        .queue_request(
            &peer.identity.public_key,
            ROOM,
            local.session_epoch(),
            mistlib_core::signaling::nostr::DiscoveryPriority::Probe,
            Duration::ZERO,
        )
        .await;
    timeout(Duration::from_secs(1), async {
        while local.senders.try_lock().is_ok() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    local.note_peer_alive(&peer.local_node_id).await;
    let released = timeout(Duration::from_millis(200), local.send_order.lock())
        .await
        .is_ok();
    local.close().await.unwrap();
    drop(blocked_wire);
    assert!(released, "confirmed peer left its obsolete pacing worker holding send_order until relay capacity or session shutdown");
}

#[tokio::test]
async fn target_connectivity_releases_blocked_worker() {
    let (local, _wire) = mock_signaler("local").await;
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    let (tx, blocked_wire) = mpsc::channel(1);
    tx.send("full".into()).await.unwrap();
    *local.senders.lock().await = vec![tx];
    let now = web_time::Instant::now();
    assert!(local
        .exchanges
        .lock()
        .await
        .request(&peer.identity.public_key, now));
    assert!(local
        .exchanges
        .lock()
        .await
        .request(&peer.identity.public_key, now + Duration::from_secs(2)));
    local
        .queue_request(
            &peer.identity.public_key,
            ROOM,
            local.session_epoch(),
            mistlib_core::signaling::nostr::DiscoveryPriority::Probe,
            Duration::ZERO,
        )
        .await;
    timeout(Duration::from_secs(1), async {
        while local.senders.try_lock().is_ok() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    local.maintain_bootstrap(2).await;
    assert!(timeout(Duration::from_millis(200), local.send_order.lock())
        .await
        .is_ok());
    local.close().await.unwrap();
    drop(blocked_wire);
}

#[tokio::test]
async fn full_relay_timeout_keeps_failed_discovery_retries_bounded() {
    let (local, _wire) = mock_signaler("local").await;
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    let key = &peer.identity.public_key;
    let (tx, mut blocked_wire) = mpsc::channel(1);
    tx.send("full".into()).await.unwrap();
    *local.senders.lock().await = vec![tx];
    let now = web_time::Instant::now();
    for attempt in 0..=3 {
        assert!(local
            .exchanges
            .lock()
            .await
            .request(key, now + Duration::from_secs(attempt * 100)));
        local
            .queue_request(
                key,
                ROOM,
                local.session_epoch(),
                mistlib_core::signaling::nostr::DiscoveryPriority::Probe,
                Duration::ZERO,
            )
            .await;
        timeout(Duration::from_millis(1500), async {
            while local.outbox.lock().await.worker_epoch.is_some() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("full relay retained the pacing worker");
        assert!(local.send_order.try_lock().is_ok());
        assert_eq!(
            local.senders.lock().await.len(),
            1,
            "timeout must retain the relay for recovery"
        );
        assert!(local.exchanges.lock().await.pending(key));
        assert!(!local.exchanges.lock().await.is_queued(key));
    }
    assert!(
        !local
            .exchanges
            .lock()
            .await
            .request(key, now + Duration::from_secs(1000)),
        "failed publications must still spend the three-retry allowance"
    );
    assert_eq!(blocked_wire.try_recv().unwrap(), "full");
    assert!(blocked_wire.try_recv().is_err());
    local.close().await.unwrap();
}

#[tokio::test]
async fn blocked_discovery_does_not_starve_answer_or_ack_on_healthy_relay() {
    let (local, _wire) = mock_signaler("local").await;
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    let key = &peer.identity.public_key;
    let (blocked, _blocked_wire) = mpsc::channel(1);
    blocked.send("full".into()).await.unwrap();
    let (healthy, mut wire) = mpsc::channel(4);
    *local.senders.lock().await = vec![blocked, healthy];
    local.publish_request_to_pubkey(key, ROOM).await.unwrap();
    timeout(Duration::from_secs(1), async {
        while local.senders.try_lock().is_ok() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    local
        .exchanges
        .lock()
        .await
        .progress(key, web_time::Instant::now());
    for kind in [SignalingType::Answer, SignalingType::NegotiationAck] {
        let outgoing = data(&local.local_node_id, &peer.local_node_id, ROOM, kind);
        timeout(
            Duration::from_millis(1500),
            local.publish_message_to_pubkey(key, &outgoing),
        )
        .await
        .expect("negotiation was starved behind obsolete discovery")
        .unwrap();
        let incoming = next_event(&mut wire).await;
        let decoded = decode_message_event(
            &local.codec_config,
            &local.crypto,
            &peer.identity,
            &peer.local_node_id,
            &incoming,
            ROOM,
        )
        .unwrap();
        assert_eq!(decoded.data.signaling_type, outgoing.signaling_type);
    }
    assert_eq!(local.senders.lock().await.len(), 2);
    local.close().await.unwrap();
}

#[tokio::test]
async fn answer_survives_temporary_full_relay() {
    negotiation_survives_temporary_full_relay(SignalingType::Answer).await;
}

#[tokio::test]
async fn ack_survives_temporary_full_relay() {
    negotiation_survives_temporary_full_relay(SignalingType::NegotiationAck).await;
}

async fn negotiation_survives_temporary_full_relay(kind: SignalingType) {
    let (local, _old_wire) = mock_signaler("local").await;
    let peer = NostrSignaler::new(NodeId("peer".into()), config());
    let key = &peer.identity.public_key;
    let (tx, mut wire) = mpsc::channel(1024);
    for _ in 0..1024 {
        tx.send("backlog".to_owned()).await.unwrap();
    }
    *local.senders.lock().await = vec![tx];
    let outgoing = data(&local.local_node_id, &peer.local_node_id, ROOM, kind);
    let recovery = async {
        tokio::time::sleep(Duration::from_millis(1250)).await;
        for _ in 0..1024 {
            assert_eq!(wire.recv().await.as_deref(), Some("backlog"));
        }
        timeout(Duration::from_millis(250), wire.recv())
            .await
            .ok()
            .flatten()
    };
    let (result, frame) = tokio::join!(local.publish_message_to_pubkey(key, &outgoing), recovery);
    local.close().await.unwrap();
    assert!(
        result.is_ok(),
        "temporary 1.25-second capacity stall discarded {:?}: {:?}; no frame after recovery={}",
        outgoing.signaling_type,
        result,
        frame.is_none()
    );
    let incoming = event(&frame.expect("negotiation frame missing after relay recovered"));
    let decoded = decode_message_event(
        &local.codec_config,
        &local.crypto,
        &peer.identity,
        &peer.local_node_id,
        &incoming,
        ROOM,
    )
    .unwrap();
    assert_eq!(decoded.data.signaling_type, outgoing.signaling_type);
}
