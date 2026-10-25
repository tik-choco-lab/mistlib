//! Coverage for `WebRtcTransport::send_queues`, the synchronous mirror of
//! `peers`' keyset introduced so `try_enqueue_send` never depends on the
//! async `peers` lock -- see `send_queues`'s own doc comment
//! (`transports/webrtc.rs`) for the tokio `RwLock` write-preferring-fairness
//! mechanism that made the old `self.peers.try_read()`-based implementation
//! drop ~1200 overlay messages/min fleet-wide despite no long-held lock
//! anywhere.
//!
//! Two things are covered here: (1) every insert/remove site keeps
//! `send_queues` in lock-step with `peers` (an invariant check per mutation
//! path), and (2) `try_enqueue_send` itself never fails while a concurrent
//! writer is hammering `peers.write().await` for an unrelated node -- the
//! exact contention shape the old implementation was vulnerable to.
use super::disconnect::{make_connected_pair, wait_for_state};
use super::*;
use bytes::Bytes;
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Exercises the `handle_offer` (answer side) and `replace_peer_and_close_old`
/// (offer side) insert sites via a real handshake: both must leave
/// `send_queues` holding exactly the same keys as `peers`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handshake_keeps_send_queues_in_lock_step_with_peers_on_both_sides() {
    let (ta, tb, id_a, id_b) = make_connected_pair();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    let a_peers: std::collections::HashSet<_> = ta.peers.read().await.keys().cloned().collect();
    let a_queues: std::collections::HashSet<_> =
        ta.send_queues.read().unwrap().keys().cloned().collect();
    assert_eq!(
        a_peers, a_queues,
        "offer side (replace_peer_and_close_old) must keep send_queues' keyset == peers' keyset"
    );

    let b_peers: std::collections::HashSet<_> = tb.peers.read().await.keys().cloned().collect();
    let b_queues: std::collections::HashSet<_> =
        tb.send_queues.read().unwrap().keys().cloned().collect();
    assert_eq!(
        b_peers, b_queues,
        "answer side (handle_offer) must keep send_queues' keyset == peers' keyset"
    );
}

/// `cleanup_session`'s unguarded remove path (`cleanup_session_impl`'s
/// `expected: None` branch) must remove from `send_queues` alongside `peers`.
#[tokio::test]
async fn cleanup_session_removes_the_matching_send_queues_entry() {
    let t = make_transport();
    let node = NodeId("cleanup-send-queues-peer".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("create_pc should succeed");

    t.peers.write().await.insert(node.clone(), peer.clone());
    t.send_queues
        .write()
        .unwrap()
        .insert(node.clone(), peer.send_tx.clone());

    t.cleanup_session(&node, false).await;

    assert!(!t.peers.read().await.contains_key(&node));
    assert!(
        !t.send_queues.read().unwrap().contains_key(&node),
        "cleanup_session must remove the send_queues entry along with the peers entry"
    );
}

/// `cleanup_session_if_current`'s identity-guarded remove path
/// (`remove_peer_if_current`) must leave `send_queues` (like `peers`)
/// untouched for a stale peer already superseded by a fresh reconnect, and
/// must remove from both once the current peer's own cleanup runs.
#[tokio::test]
async fn cleanup_session_if_current_keeps_send_queues_in_lock_step_with_peers() {
    let t = make_transport();
    let node = NodeId("guarded-send-queues-peer".to_string());

    let old_peer = t
        .create_pc(node.clone())
        .await
        .expect("old peer should be created");
    let new_peer = t
        .create_pc(node.clone())
        .await
        .expect("new peer should be created");

    t.peers.write().await.insert(node.clone(), old_peer.clone());
    t.send_queues
        .write()
        .unwrap()
        .insert(node.clone(), old_peer.send_tx.clone());

    // A fresh reconnect wins the race, mirroring `replace_peer_and_close_old`/
    // `handle_offer`'s insert pair.
    t.peers.write().await.insert(node.clone(), new_peer.clone());
    t.send_queues
        .write()
        .unwrap()
        .insert(node.clone(), new_peer.send_tx.clone());

    // The stale attempt's cleanup must be a complete no-op on both maps.
    let old_expected = Arc::downgrade(&old_peer);
    t.cleanup_session_if_current(&node, &old_expected, true, "test_stale")
        .await;
    assert!(
        matches!(t.peers.read().await.get(&node), Some(p) if Arc::ptr_eq(p, &new_peer)),
        "a stale cleanup must not remove the new peer's registration"
    );
    assert!(
        t.send_queues.read().unwrap().contains_key(&node),
        "a stale cleanup must not remove the new peer's send_queues entry"
    );

    // The current attempt's own cleanup must remove both.
    let new_expected = Arc::downgrade(&new_peer);
    t.cleanup_session_if_current(&node, &new_expected, true, "test_current")
        .await;
    assert!(!t.peers.read().await.contains_key(&node));
    assert!(
        !t.send_queues.read().unwrap().contains_key(&node),
        "the current peer's own cleanup must remove its send_queues entry too"
    );
}

/// `close_all_peer_connections`'s full-clear path must clear `send_queues`
/// alongside `peers`.
#[tokio::test]
async fn close_all_peer_connections_clears_send_queues_too() {
    let t = make_transport();
    let node = NodeId("close-all-send-queues-peer".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("create_pc should succeed");

    t.peers.write().await.insert(node.clone(), peer.clone());
    t.send_queues
        .write()
        .unwrap()
        .insert(node.clone(), peer.send_tx.clone());

    t.close_all_peer_connections().await;

    assert!(t.peers.read().await.is_empty());
    assert!(
        t.send_queues.read().unwrap().is_empty(),
        "close_all_peer_connections must clear send_queues along with peers"
    );
}

/// The core regression this whole fix is for: `try_enqueue_send` must never
/// fail while some unrelated node's registration is being churned through
/// `peers.write().await` on another task. Under the old implementation
/// (`self.peers.try_read()`), tokio's write-preferring `RwLock` fairness
/// fails EVERY concurrent `try_read()` while a writer is queued -- even
/// though each writer here only ever holds the lock for a brief,
/// non-blocking insert/remove -- which is what measured as ~1200 dropped
/// overlay messages/min fleet-wide. `try_enqueue_send` no longer touches
/// `self.peers` at all, so this must hold unconditionally now.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn try_enqueue_send_never_fails_while_a_concurrent_writer_hammers_peers() {
    let t = Arc::new(make_transport());
    let node = NodeId("enqueue-under-contention-peer".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("create_pc should succeed");
    t.peers.write().await.insert(node.clone(), peer.clone());
    t.send_queues
        .write()
        .unwrap()
        .insert(node.clone(), peer.send_tx.clone());

    let unrelated = NodeId("unrelated-contention-peer".to_string());
    let unrelated_peer = t
        .create_pc(unrelated.clone())
        .await
        .expect("create_pc should succeed");

    let stop = Arc::new(AtomicBool::new(false));
    let writer_t = t.clone();
    let writer_stop = stop.clone();
    let writer = tokio::spawn(async move {
        while !writer_stop.load(Ordering::Relaxed) {
            writer_t
                .peers
                .write()
                .await
                .insert(unrelated.clone(), unrelated_peer.clone());
            writer_t.peers.write().await.remove(&unrelated);
        }
    });

    // Stay well under `PEER_SEND_QUEUE_CAPACITY` (256; already covered by
    // `reorder::send_queue_rejects_once_capacity_is_exceeded`) so this can
    // never fail on queue-full regardless of how fast `Peer::spawn_send_queue`'s
    // drainer gets scheduled under the test binary's own CPU contention --
    // the only failure mode this test is meant to catch is
    // `try_enqueue_send` erroring because `send_queues`'s lock was
    // (wrongly) unavailable.
    const ATTEMPTS: u32 = 100;
    for i in 0..ATTEMPTS {
        t.try_enqueue_send(
            &node,
            Bytes::copy_from_slice(&i.to_be_bytes()),
            DeliveryMethod::ReliableOrdered,
        )
        .expect("try_enqueue_send must never fail due to peers-lock contention");
    }

    stop.store(true, Ordering::Relaxed);
    writer.await.unwrap();
}

#[tokio::test]
async fn awaitable_send_waits_for_capacity_and_frame_completion() {
    use crate::transports::webrtc::peer::QueuedSend;
    let t = make_transport();
    let node = NodeId("receipt-peer".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(QueuedSend {
        data: Bytes::from_static(b"first"),
        method: DeliveryMethod::ReliableOrdered,
        completion: None,
        _telemetry: None,
        _bulk_permit: None,
    })
    .unwrap();
    t.send_queues.write().unwrap().insert(node.clone(), tx);
    let send = t.send_wait(
        &node,
        Bytes::from_static(b"second"),
        DeliveryMethod::ReliableOrdered,
    );
    tokio::pin!(send);
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut send)
        .await
        .is_err());
    assert_eq!(rx.recv().await.unwrap().data, Bytes::from_static(b"first"));
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut send)
        .await
        .is_err());
    let queued = rx.recv().await.unwrap();
    assert_eq!(queued.data, Bytes::from_static(b"second"));
    queued.completion.unwrap().send(Ok(())).unwrap();
    assert!(send.await.is_ok());
}

#[tokio::test]
async fn awaitable_send_reports_write_failure_and_queue_teardown() {
    let t = make_transport();
    let node = NodeId("receipt-peer".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    t.send_queues.write().unwrap().insert(node.clone(), tx);
    let send = t.send_wait(
        &node,
        Bytes::from_static(b"fail"),
        DeliveryMethod::ReliableOrdered,
    );
    tokio::pin!(send);
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut send)
        .await
        .is_err());
    rx.recv()
        .await
        .unwrap()
        .completion
        .unwrap()
        .send(Err(mistlib_core::error::MistError::Internal(
            "write failure".into(),
        )))
        .unwrap();
    assert!(send
        .await
        .unwrap_err()
        .to_string()
        .contains("write failure"));
    drop(rx);
    assert!(t
        .send_wait(
            &node,
            Bytes::from_static(b"closed"),
            DeliveryMethod::ReliableOrdered
        )
        .await
        .is_err());
    assert!(t
        .send_wait(
            &NodeId("missing".into()),
            Bytes::new(),
            DeliveryMethod::ReliableOrdered
        )
        .await
        .is_err());
}

#[tokio::test]
async fn awaitable_send_errors_when_admitted_frame_is_discarded() {
    let t = make_transport();
    let node = NodeId("receipt-peer".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    t.send_queues.write().unwrap().insert(node.clone(), tx);
    let send = t.send_wait(
        &node,
        Bytes::from_static(b"discarded"),
        DeliveryMethod::ReliableOrdered,
    );
    tokio::pin!(send);
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut send)
        .await
        .is_err());
    drop(rx.recv().await.unwrap());
    assert!(send.await.is_err());
}

#[tokio::test]
async fn w5_regression_cancel_before_admission_must_not_consume_overlay_sequence() {
    use crate::engine::tests::session_registry::{
        session_with_webrtc_transport, REGISTRY_TEST_LOCK,
    };
    use crate::engine::ENGINE;
    use crate::transports::webrtc::peer::QueuedSend;
    use mistlib_core::overlay::OverlayRouter;
    let _guard = REGISTRY_TEST_LOCK.lock().await;
    let transport = Arc::new(make_transport());
    let peer = NodeId("review-peer".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(QueuedSend {
        data: Bytes::new(),
        method: DeliveryMethod::ReliableOrdered,
        completion: None,
        _telemetry: None,
        _bulk_permit: None,
    })
    .unwrap();
    transport
        .send_queues
        .write()
        .unwrap()
        .insert(peer.clone(), tx);
    let mut session = session_with_webrtc_transport("review-room", transport);
    let overlay = Arc::new(OverlayRouter::new(
        &mistlib_core::config::Config::default(),
        session.node_store.clone(),
        NodeId("sender".into()),
    ));
    Arc::get_mut(&mut session).unwrap().overlay = Some(overlay.clone());
    ENGINE
        .sessions
        .write()
        .unwrap()
        .push(("review-room".into(), session));
    let cancelled = tokio::time::timeout(
        Duration::from_millis(20),
        crate::app::send_message_direct_wait_in_room(
            "review-room".into(),
            peer.0.clone(),
            b"cancelled".to_vec(),
            crate::app::DELIVERY_RELIABLE,
        ),
    )
    .await;
    assert!(cancelled.is_err());
    rx.recv().await.unwrap();
    let success = crate::app::send_message_direct_wait_in_room(
        "review-room".into(),
        peer.0.clone(),
        b"next".to_vec(),
        crate::app::DELIVERY_RELIABLE,
    );
    tokio::pin!(success);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut success)
            .await
            .is_err()
    );
    let frame = rx.recv().await.unwrap();
    let envelope: mistlib_core::overlay::OverlayEnvelope =
        mistlib_core::overlay::wire::deserialize(&frame.data).unwrap();
    frame.completion.unwrap().send(Ok(())).unwrap();
    assert!(success.await.is_ok());
    ENGINE
        .sessions
        .write()
        .unwrap()
        .retain(|(id, _)| id != "review-room");
    let receiver = OverlayRouter::new(
        &mistlib_core::config::Config::default(),
        overlay.node_store.clone(),
        peer,
    );
    let delivered = receiver.reorder_inbound(&envelope.from, envelope.seq, envelope.content);
    assert_eq!(
        delivered.len(),
        1,
        "successful next frame has seq {}; cancelling before admission left a gap (8s stall)",
        envelope.seq
    );
}

#[tokio::test]
async fn rejected_oversized_send_must_not_stall_next_valid_send() {
    use crate::engine::tests::session_registry::{
        session_with_webrtc_transport, REGISTRY_TEST_LOCK,
    };
    use crate::engine::ENGINE;
    use mistlib_core::overlay::OverlayRouter;
    let _guard = REGISTRY_TEST_LOCK.lock().await;
    let transport = Arc::new(make_transport());
    let peer = NodeId("review-peer".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    transport
        .send_queues
        .write()
        .unwrap()
        .insert(peer.clone(), tx);
    let mut session = session_with_webrtc_transport("review-room", transport.clone());
    let overlay = Arc::new(OverlayRouter::new(
        &mistlib_core::config::Config::default(),
        session.node_store.clone(),
        NodeId("sender".into()),
    ));
    Arc::get_mut(&mut session).unwrap().overlay = Some(overlay.clone());
    ENGINE
        .sessions
        .write()
        .unwrap()
        .push(("review-room".into(), session));
    transport.set_max_message_bytes(64);
    let rejected = crate::app::send_message_direct_wait_in_room(
        "review-room".into(),
        peer.0.clone(),
        vec![0; 64],
        crate::app::DELIVERY_RELIABLE,
    )
    .await;
    assert!(
        matches!(rejected, Err(crate::error::MistError::Core(mistlib_core::error::MistError::MessageTooLarge { size, limit: 64 })) if size > 64)
    );
    assert!(rx.try_recv().is_err(), "rejected frame must not be queued");
    let success = crate::app::send_message_direct_wait_in_room(
        "review-room".into(),
        peer.0.clone(),
        b"next".to_vec(),
        crate::app::DELIVERY_RELIABLE,
    );
    tokio::pin!(success);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut success)
            .await
            .is_err()
    );
    let frame = rx.recv().await.unwrap();
    let envelope: mistlib_core::overlay::OverlayEnvelope =
        mistlib_core::overlay::wire::deserialize(&frame.data).unwrap();
    frame.completion.unwrap().send(Ok(())).unwrap();
    assert!(success.await.is_ok());
    ENGINE
        .sessions
        .write()
        .unwrap()
        .retain(|(id, _)| id != "review-room");
    let receiver = OverlayRouter::new(
        &mistlib_core::config::Config::default(),
        overlay.node_store.clone(),
        peer,
    );
    let delivered = receiver.reorder_inbound(&envelope.from, envelope.seq, envelope.content);
    assert_eq!(
        delivered.len(),
        1,
        "successful next frame has seq {}; rejecting an oversized frame left a gap (8s stall)",
        envelope.seq
    );
}

#[tokio::test]
async fn direct_send_rejections_must_not_consume_overlay_sequence() {
    use crate::engine::tests::session_registry::{
        session_with_webrtc_transport, REGISTRY_TEST_LOCK,
    };
    use crate::engine::ENGINE;
    use mistlib_core::overlay::OverlayRouter;
    let _guard = REGISTRY_TEST_LOCK.lock().await;
    let transport = Arc::new(make_transport());
    let peer = NodeId("review-peer".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    transport
        .send_queues
        .write()
        .unwrap()
        .insert(peer.clone(), tx.clone());
    let mut session = session_with_webrtc_transport("review-room", transport.clone());
    let overlay = Arc::new(OverlayRouter::new(
        &mistlib_core::config::Config::default(),
        session.node_store.clone(),
        NodeId("sender".into()),
    ));
    Arc::get_mut(&mut session).unwrap().overlay = Some(overlay.clone());
    ENGINE
        .sessions
        .write()
        .unwrap()
        .push(("review-room".into(), session));
    transport.send_queues.write().unwrap().remove(&peer);
    assert!(
        crate::app::send_message_direct(peer.0.clone(), vec![], crate::app::DELIVERY_RELIABLE)
            .await
            .is_err()
    );
    let (closed_tx, closed_rx) = tokio::sync::mpsc::channel(1);
    drop(closed_rx);
    transport
        .send_queues
        .write()
        .unwrap()
        .insert(peer.clone(), closed_tx);
    assert!(
        crate::app::send_message_direct(peer.0.clone(), vec![], crate::app::DELIVERY_RELIABLE)
            .await
            .is_err()
    );
    transport
        .send_queues
        .write()
        .unwrap()
        .insert(peer.clone(), tx.clone());
    tx.try_send(crate::transports::webrtc::peer::QueuedSend {
        data: Bytes::new(),
        method: DeliveryMethod::ReliableOrdered,
        completion: None,
        _telemetry: None,
        _bulk_permit: None,
    })
    .unwrap();
    assert!(
        crate::app::send_message_direct(peer.0.clone(), vec![], crate::app::DELIVERY_RELIABLE)
            .await
            .is_err()
    );
    rx.recv().await.unwrap();
    transport.set_max_message_bytes(64);
    let rejected =
        crate::app::send_message_direct(peer.0.clone(), vec![0; 64], crate::app::DELIVERY_RELIABLE)
            .await;
    assert!(
        matches!(rejected, Err(crate::error::MistError::Core(mistlib_core::error::MistError::MessageTooLarge { size, limit: 64 })) if size > 64)
    );
    assert!(rx.try_recv().is_err(), "rejected frame must not be queued");
    crate::app::send_message_direct(
        peer.0.clone(),
        b"next".to_vec(),
        crate::app::DELIVERY_RELIABLE,
    )
    .await
    .unwrap();
    let frame = rx.recv().await.unwrap();
    assert!(frame.completion.is_none());
    let envelope: mistlib_core::overlay::OverlayEnvelope =
        mistlib_core::overlay::wire::deserialize(&frame.data).unwrap();
    ENGINE
        .sessions
        .write()
        .unwrap()
        .retain(|(id, _)| id != "review-room");
    let receiver = OverlayRouter::new(
        &mistlib_core::config::Config::default(),
        overlay.node_store.clone(),
        peer,
    );
    let delivered = receiver.reorder_inbound(&envelope.from, envelope.seq, envelope.content);
    assert_eq!(
        delivered.len(),
        1,
        "successful next frame has seq {}; rejecting an oversized frame left a gap (8s stall)",
        envelope.seq
    );
}

/// Exercise the production session handler and both app admission APIs against
/// the same queue, without sockets or a DataChannel drainer hiding enqueue order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_app_and_overlay_sends_enqueue_in_sequence_order() {
    use crate::engine::tests::session_registry::{
        session_with_webrtc_transport, REGISTRY_TEST_LOCK,
    };
    use crate::engine::ENGINE;
    use crate::layers::native_l0::init::SessionActionHandler;
    use mistlib_core::overlay::{OverlayEnvelope, OverlayRouter, OverlayTransport};

    const WORKERS: usize = 9;
    const SENDS: usize = 100;
    const ROOM: &str = "atomic-enqueue-room";
    let _guard = REGISTRY_TEST_LOCK.lock().await;
    let transport = Arc::new(make_transport());
    let peer = NodeId("atomic-enqueue-peer".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(WORKERS * SENDS);
    transport
        .send_queues
        .write()
        .unwrap()
        .insert(peer.clone(), tx);
    let mut session = session_with_webrtc_transport(ROOM, transport);
    let router = Arc::new(OverlayRouter::new(
        &mistlib_core::config::Config::default(),
        session.node_store.clone(),
        NodeId("sender".into()),
    ));
    Arc::get_mut(&mut session).unwrap().overlay = Some(router.clone());
    ENGINE
        .sessions
        .write()
        .unwrap()
        .push((ROOM.into(), session));
    let overlay = Arc::new(OverlayTransport {
        router,
        action_handler: Arc::new(SessionActionHandler {
            room_id: ROOM.into(),
        }),
    });
    let barrier = Arc::new(tokio::sync::Barrier::new(WORKERS));
    let mut workers = Vec::new();
    for worker in 0..WORKERS {
        let peer = peer.clone();
        let overlay = overlay.clone();
        let barrier = barrier.clone();
        workers.push(tokio::spawn(async move {
            barrier.wait().await;
            for _ in 0..SENDS {
                match worker % 3 {
                    0 => crate::app::send_message_direct(
                        peer.0.clone(),
                        b"direct".to_vec(),
                        crate::app::DELIVERY_RELIABLE,
                    )
                    .await
                    .unwrap(),
                    1 => crate::app::send_message_direct_wait_in_room(
                        ROOM.into(),
                        peer.0.clone(),
                        b"wait".to_vec(),
                        crate::app::DELIVERY_RELIABLE,
                    )
                    .await
                    .unwrap(),
                    _ => overlay
                        .send(
                            &peer,
                            Bytes::from_static(b"overlay"),
                            DeliveryMethod::ReliableOrdered,
                        )
                        .await
                        .unwrap(),
                }
                tokio::task::yield_now().await;
            }
        }));
    }
    let mut sequences = Vec::new();
    for _ in 0..WORKERS * SENDS {
        let queued = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("all sends must reach the queue")
            .unwrap();
        let envelope: OverlayEnvelope =
            mistlib_core::overlay::wire::deserialize(&queued.data).unwrap();
        sequences.push(envelope.seq);
        if let Some(completion) = queued.completion {
            completion.send(Ok(())).unwrap();
        }
    }
    for worker in workers {
        worker.await.unwrap();
    }
    ENGINE
        .sessions
        .write()
        .unwrap()
        .retain(|(id, _)| id != ROOM);
    assert_eq!(
        sequences,
        (1..=(WORKERS * SENDS) as u64).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn overlay_admission_errors_do_not_consume_sequence() {
    use crate::engine::tests::session_registry::{
        session_with_webrtc_transport, REGISTRY_TEST_LOCK,
    };
    use crate::engine::ENGINE;
    use crate::layers::native_l0::init::SessionActionHandler;
    use mistlib_core::overlay::{OverlayEnvelope, OverlayRouter, OverlayTransport};

    const ROOM: &str = "overlay-admission-room";
    let _guard = REGISTRY_TEST_LOCK.lock().await;
    let transport = Arc::new(make_transport());
    let peer = NodeId("admission-peer".into());
    let mut session = session_with_webrtc_transport(ROOM, transport.clone());
    let router = Arc::new(OverlayRouter::new(
        &mistlib_core::config::Config::default(),
        session.node_store.clone(),
        NodeId("sender".into()),
    ));
    Arc::get_mut(&mut session).unwrap().overlay = Some(router.clone());
    let overlay = OverlayTransport {
        router,
        action_handler: Arc::new(SessionActionHandler {
            room_id: ROOM.into(),
        }),
    };
    // Session gone, peer absent, closed queue, full queue, and oversized frame.
    assert!(overlay
        .send(&peer, Bytes::new(), DeliveryMethod::ReliableOrdered)
        .await
        .is_err());
    ENGINE
        .sessions
        .write()
        .unwrap()
        .push((ROOM.into(), session));
    assert!(overlay
        .send(&peer, Bytes::new(), DeliveryMethod::ReliableOrdered)
        .await
        .is_err());
    let (closed_tx, closed_rx) = tokio::sync::mpsc::channel(1);
    drop(closed_rx);
    transport
        .send_queues
        .write()
        .unwrap()
        .insert(peer.clone(), closed_tx);
    assert!(overlay
        .send(&peer, Bytes::new(), DeliveryMethod::ReliableOrdered)
        .await
        .is_err());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(crate::transports::webrtc::peer::QueuedSend {
        data: Bytes::new(),
        method: DeliveryMethod::ReliableOrdered,
        completion: None,
        _telemetry: None,
        _bulk_permit: None,
    })
    .unwrap();
    transport
        .send_queues
        .write()
        .unwrap()
        .insert(peer.clone(), tx);
    assert!(overlay
        .send(&peer, Bytes::new(), DeliveryMethod::ReliableOrdered)
        .await
        .is_err());
    rx.recv().await.unwrap();
    transport.set_max_message_bytes(64);
    assert!(overlay
        .send(
            &peer,
            Bytes::from(vec![0; 64]),
            DeliveryMethod::ReliableOrdered
        )
        .await
        .is_err());
    overlay
        .send(&peer, Bytes::new(), DeliveryMethod::ReliableOrdered)
        .await
        .unwrap();
    let queued = rx.recv().await.unwrap();
    let envelope: OverlayEnvelope = mistlib_core::overlay::wire::deserialize(&queued.data).unwrap();
    assert_eq!(envelope.seq, 1);
    transport
        .connection_states
        .write()
        .unwrap()
        .insert(peer.clone(), ConnectionState::Connected);
    transport
        .try_enqueue_send(&peer, Bytes::new(), DeliveryMethod::ReliableOrdered)
        .unwrap();
    assert!(overlay
        .broadcast(Bytes::new(), DeliveryMethod::ReliableOrdered)
        .await
        .is_err());
    rx.recv().await.unwrap();
    overlay
        .broadcast(Bytes::new(), DeliveryMethod::ReliableOrdered)
        .await
        .unwrap();
    let broadcast = rx.recv().await.unwrap();
    let envelope: OverlayEnvelope =
        mistlib_core::overlay::wire::deserialize(&broadcast.data).unwrap();
    assert_eq!(envelope.seq, 0);
    ENGINE
        .sessions
        .write()
        .unwrap()
        .retain(|(id, _)| id != ROOM);
}

#[tokio::test]
async fn reliable_unordered_budget_waits_for_active_frame_and_is_bound_to_queue_generation() {
    use mistlib_core::action::{OverlayAction, SendLane};
    let transport = make_transport();
    let config = mistlib_core::config::StorageTransferConfig {
        control_priority: true,
        bulk_queue_bytes: 100,
        ..Default::default()
    };
    transport.set_storage_transfer(config);
    let node = NodeId("bulk-budget".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    transport
        .send_queues
        .write()
        .unwrap()
        .insert(node.clone(), tx);
    let reserved = transport
        .reserve_overlay_send(&node, 80, SendLane::Bulk)
        .await
        .unwrap();
    let receipt = reserved(OverlayAction::SendMessage {
        to: node.clone(),
        data: Bytes::from(vec![0; 80]),
        method: DeliveryMethod::ReliableOrdered,
        lane: SendLane::Bulk,
    })
    .unwrap();
    let frame = rx.recv().await.unwrap();
    let waiting = transport.reserve_overlay_send(&node, 30, SendLane::Bulk);
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut waiting)
            .await
            .is_err()
    );
    // Dequeuing is insufficient: an active frame still owns its byte permit.
    drop(frame);
    let reserved = waiting.await.unwrap();
    drop(reserved);
    assert!(receipt.await.is_err());
    let (new_tx, _new_rx) = tokio::sync::mpsc::channel(8);
    transport
        .send_queues
        .write()
        .unwrap()
        .insert(node.clone(), new_tx);
    assert!(transport
        .reserve_overlay_send(&node, 100, SendLane::Bulk)
        .await
        .is_ok());
}

#[test]
fn native_queue_prefers_control_without_skipping_positive_bulk() {
    use mistlib_core::overlay::OverlayEnvelope;
    use mistlib_core::signaling::MessageContent;
    use mistlib_core::transport::send::{LaneQueue, ScheduledSend};
    let cid = mistlib_core::storage::compute_cid(b"data", 0x55);
    let frame = |seq, payload: Vec<u8>| crate::transports::webrtc::peer::QueuedSend {
        data: mistlib_core::overlay::wire::serialize(
            &OverlayEnvelope::new(
                NodeId("a".into()),
                NodeId("b".into()),
                1,
                MessageContent::Raw(payload.into()),
            )
            .with_seq(seq),
        )
        .unwrap()
        .into(),
        method: DeliveryMethod::ReliableOrdered,
        completion: None,
        _telemetry: None,
        _bulk_permit: None,
    };
    for seq in [0, 1] {
        let mut queue = LaneQueue::new(true);
        queue.push_back(frame(
            seq,
            mistlib_core::storage::protocol::build_have_chunk_message(&cid, 0, 1, b"data"),
        ));
        queue.push_back(frame(
            2,
            mistlib_core::storage::protocol::build_want_message(&cid),
        ));
        let (first, bulk) = queue.pop(true, true).unwrap();
        assert_eq!(
            first.send_class().lane,
            if seq == 0 {
                mistlib_core::action::SendLane::Control
            } else {
                mistlib_core::action::SendLane::Bulk
            }
        );
        assert!(!bulk);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn negotiated_reliable_unordered_is_unordered_reliable_and_close_preserves_peer() {
    use mistlib_core::{overlay::OverlayEnvelope, signaling::MessageContent};
    use webrtc::data_channel::data_channel_state::RTCDataChannelState;
    let (a, b, id_a, id_b) = make_connected_pair();
    let config = mistlib_core::config::StorageTransferConfig {
        bulk_channel: true,
        unsequenced_bulk: true,
        control_priority: true,
        ..Default::default()
    };
    a.set_storage_transfer(config.clone());
    b.set_storage_transfer(config);
    a.connect(&id_b).await.unwrap();
    assert!(wait_for_state(&a, &id_b, ConnectionState::Connected, 10_000).await);
    assert!(wait_for_state(&b, &id_a, ConnectionState::Connected, 10_000).await);
    let peer_a = a.peers.read().await.get(&id_b).cloned().unwrap();
    let peer_b = b.peers.read().await.get(&id_a).cloned().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let open_a = peer_a
                .unordered_channel
                .read()
                .await
                .as_ref()
                .is_some_and(|dc| dc.ready_state() == RTCDataChannelState::Open);
            let open_b = peer_b
                .unordered_channel
                .read()
                .await
                .as_ref()
                .is_some_and(|dc| dc.ready_state() == RTCDataChannelState::Open);
            if open_a && open_b {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let bulk = peer_a.unordered_channel.read().await.clone().unwrap();
    assert_eq!(bulk.label(), "reliable-unordered");
    assert!(!bulk.ordered());
    assert_eq!(bulk.max_retransmits(), None);
    assert_eq!(bulk.max_packet_lifetime(), None);
    let reliable = peer_a
        .channels
        .write()
        .await
        .remove(&DeliveryMethod::ReliableOrdered)
        .unwrap();
    let cid = mistlib_core::storage::compute_cid(b"data", 0x55);
    let envelope = OverlayEnvelope::new(
        id_a.clone(),
        id_b.clone(),
        1,
        MessageContent::Raw(
            mistlib_core::storage::protocol::build_have_chunk_message(&cid, 0, 1, b"data").into(),
        ),
    );
    a.send_wait(
        &id_b,
        mistlib_core::overlay::wire::serialize(&envelope)
            .unwrap()
            .into(),
        DeliveryMethod::ReliableUnordered,
    )
    .await
    .unwrap();
    assert!(a
        .send_wait(
            &id_b,
            mistlib_core::overlay::wire::serialize(&envelope.clone().with_seq(3))
                .unwrap()
                .into(),
            DeliveryMethod::ReliableOrdered
        )
        .await
        .is_err());
    peer_a
        .channels
        .write()
        .await
        .insert(DeliveryMethod::ReliableOrdered, reliable);
    let reliable_b = peer_b
        .channels
        .read()
        .await
        .get(&DeliveryMethod::ReliableOrdered)
        .cloned()
        .unwrap();
    let unknown = peer_a
        .pc
        .create_data_channel("unknown-future-label", None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(Arc::ptr_eq(
        &reliable_b,
        peer_b
            .channels
            .read()
            .await
            .get(&DeliveryMethod::ReliableOrdered)
            .unwrap()
    ));
    let _ = unknown.close().await;
    bulk.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while peer_a.unordered_channel.read().await.is_some()
            || peer_b.unordered_channel.read().await.is_some()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(a.get_connection_state(&id_b), ConnectionState::Connected);
    assert_eq!(b.get_connection_state(&id_a), ConnectionState::Connected);
    a.send_wait(
        &id_b,
        mistlib_core::overlay::wire::serialize(&envelope)
            .unwrap()
            .into(),
        DeliveryMethod::ReliableUnordered,
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_bulk_flags_still_negotiate_public_reliable_unordered() {
    let (a, b, id_a, id_b) = make_connected_pair();
    // A uses CP50 defaults; B explicitly uses the legacy storage policy.
    b.set_storage_transfer(mistlib_core::config::StorageTransferConfig {
        unsequenced_bulk: false,
        control_priority: false,
        bulk_channel: false,
        bulk_target_delay_ms: 0,
        ..Default::default()
    });
    struct Recorder(tokio::sync::mpsc::UnboundedSender<Bytes>);
    impl mistlib_core::transport::NetworkEventHandler for Recorder {
        fn on_event(&self, event: mistlib_core::transport::NetworkEvent) {
            self.0.send(event.data).unwrap();
        }
    }
    let (tx_a, mut rx_a) = tokio::sync::mpsc::unbounded_channel();
    let (tx_b, mut rx_b) = tokio::sync::mpsc::unbounded_channel();
    a.start(Arc::new(Recorder(tx_a))).await.unwrap();
    b.start(Arc::new(Recorder(tx_b))).await.unwrap();
    a.connect(&id_b).await.unwrap();
    assert!(wait_for_state(&a, &id_b, ConnectionState::Connected, 10_000).await);
    assert!(wait_for_state(&b, &id_a, ConnectionState::Connected, 10_000).await);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let pa = a.peers.read().await.get(&id_b).cloned().unwrap();
            let pb = b.peers.read().await.get(&id_a).cloned().unwrap();
            if pa.unordered_channel.read().await.is_some()
                && pb.unordered_channel.read().await.is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let cid = mistlib_core::storage::compute_cid(b"data", 0x55);
    let chunk = mistlib_core::storage::protocol::build_have_chunk_message(&cid, 0, 1, b"data");
    // Exercise each direction with the source router's actual storage policy,
    // then a control frame. Receive assertions catch silent drops.
    for (sender, from, to, receiver, legacy) in [
        (&a, &id_a, &id_b, &mut rx_b, false),
        (&b, &id_b, &id_a, &mut rx_a, true),
    ] {
        let mut config = mistlib_core::config::Config::default();
        if legacy {
            config.storage.transfer = b.storage_transfer.read().unwrap().clone();
        }
        let router = mistlib_core::overlay::OverlayRouter::new(
            &config,
            Arc::new(std::sync::Mutex::new(
                mistlib_core::overlay::node_store::NodeStore::new(),
            )),
            from.clone(),
        );
        for (index, payload) in [
            chunk.clone(),
            mistlib_core::storage::protocol::build_want_message(&cid),
        ]
        .into_iter()
        .enumerate()
        {
            let mistlib_core::action::OverlayAction::SendMessage { data, method, .. } =
                router.wrap_data(to, payload.into(), DeliveryMethod::ReliableOrdered)
            else {
                panic!("expected send")
            };
            let envelope: mistlib_core::overlay::OverlayEnvelope =
                mistlib_core::overlay::wire::deserialize(&data).unwrap();
            let is_bulk = index == 0;
            assert_eq!(
                envelope.seq,
                if legacy {
                    index as u64 + 1
                } else {
                    index as u64
                }
            );
            assert_eq!(
                method,
                if is_bulk && !legacy {
                    DeliveryMethod::ReliableUnordered
                } else {
                    DeliveryMethod::ReliableOrdered
                }
            );
            sender.send_wait(to, data.clone(), method).await.unwrap();
            let received = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(received, data);
        }
    }
}

// Uses only mpsc and byte permits; no PeerConnection or sockets.
#[tokio::test]
async fn received_bulk_admission_retains_positive_and_zero_sequences_under_pressure() {
    use mistlib_core::{overlay::OverlayEnvelope, signaling::MessageContent};
    for seq in [0, 7] {
        let t = make_transport();
        t.set_storage_transfer(mistlib_core::config::StorageTransferConfig {
            control_priority: true,
            bulk_queue_bytes: 128,
            ..Default::default()
        });
        let node = NodeId("relay-egress".into());
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        t.send_queues
            .write()
            .unwrap()
            .insert(node.clone(), tx.clone());
        let active = t
            .bulk_budget(&node, &tx)
            .unwrap()
            .reserve(128)
            .await
            .unwrap();
        let cid = mistlib_core::storage::compute_cid(b"chunk", 0x55);
        let data: Bytes = mistlib_core::overlay::wire::serialize(
            &OverlayEnvelope::new(
                NodeId("source".into()),
                NodeId("destination".into()),
                2,
                MessageContent::Raw(
                    mistlib_core::storage::protocol::build_have_chunk_message(
                        &cid, 0, 1, &[0; 200],
                    )
                    .into(),
                ),
            )
            .with_seq(seq),
        )
        .unwrap()
        .into();
        // Received frames larger than the relay's budget must survive too.
        let send = t.enqueue_received_send(&node, data.clone(), DeliveryMethod::ReliableOrdered);
        tokio::pin!(send);
        assert!(futures::poll!(&mut send).is_pending());
        assert!(rx.try_recv().is_err());
        drop(active);
        send.await.unwrap();
        let first = rx.recv().await.unwrap();
        assert_eq!(first.data, data);
        let second = t.enqueue_received_send(&node, data.clone(), DeliveryMethod::ReliableOrdered);
        tokio::pin!(second);
        assert!(
            futures::poll!(&mut second).is_pending(),
            "active frame still owns its budget"
        );
        drop(first);
        second.await.unwrap();
        assert_eq!(rx.recv().await.unwrap().data, data);
    }
}

#[tokio::test]
async fn retained_callbacks_keep_fifo_and_never_replay_to_replacement_queues() {
    let t = make_transport();
    let node = NodeId("retained-callbacks".into());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    t.send_queues.write().unwrap().insert(node.clone(), tx);
    let first = t.retain_received_send(
        &node,
        Bytes::from_static(b"first"),
        DeliveryMethod::ReliableOrdered,
    );
    let second = t.retain_received_send(
        &node,
        Bytes::from_static(b"second"),
        DeliveryMethod::ReliableOrdered,
    );
    tokio::pin!(first, second);
    assert!(futures::poll!(&mut second).is_pending());
    first.await.unwrap();
    assert!(
        futures::poll!(&mut second).is_pending(),
        "full ingress queue must backpressure"
    );
    assert_eq!(rx.recv().await.unwrap().data.as_ref(), b"first");
    second.await.unwrap();
    assert_eq!(rx.recv().await.unwrap().data.as_ref(), b"second");

    let stale = t.retain_received_send(
        &node,
        Bytes::from_static(b"old-pc"),
        DeliveryMethod::ReliableOrdered,
    );
    let (new_tx, mut new_rx) = tokio::sync::mpsc::channel(1);
    t.send_queues.write().unwrap().insert(node, new_tx);
    drop(rx);
    assert!(stale.await.is_err());
    assert!(new_rx.try_recv().is_err());
}

#[tokio::test]
async fn retained_broadcast_keeps_per_peer_order_without_blocking_other_peers() {
    let t = make_transport();
    let mut receivers = Vec::new();
    for name in ["first", "second"] {
        let node = NodeId(name.into());
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        t.send_queues.write().unwrap().insert(node.clone(), tx);
        t.connection_states
            .write()
            .unwrap()
            .insert(node, ConnectionState::Connected);
        receivers.push(rx);
    }
    t.retain_received_send(
        &NodeId("first".into()),
        Bytes::from_static(b"older"),
        DeliveryMethod::ReliableOrdered,
    )
    .await
    .unwrap();
    let broadcast = t.retain_received_send(
        &NodeId::broadcast(),
        Bytes::from_static(b"broadcast"),
        DeliveryMethod::ReliableOrdered,
    );
    tokio::pin!(broadcast);
    assert!(futures::poll!(&mut broadcast).is_pending());
    assert_eq!(
        receivers[1].recv().await.unwrap().data.as_ref(),
        b"broadcast"
    );
    assert_eq!(receivers[0].recv().await.unwrap().data.as_ref(), b"older");
    broadcast.await.unwrap();
    assert_eq!(
        receivers[0].recv().await.unwrap().data.as_ref(),
        b"broadcast"
    );
}

// Socket-free: uses the same queued frame type and scheduler as the drainer.
#[test]
fn raw_application_ping_runs_while_unordered_bulk_write_is_blocked() {
    use mistlib_core::transport::send::LaneQueue;
    let cid = mistlib_core::storage::compute_cid(b"data", 0x55);
    let wire =
        mistlib_core::overlay::wire::serialize(&mistlib_core::overlay::OverlayEnvelope::new(
            NodeId("a".into()),
            NodeId("b".into()),
            1,
            mistlib_core::signaling::MessageContent::Raw(
                mistlib_core::storage::protocol::build_have_chunk_message(&cid, 0, 1, b"data")
                    .into(),
            ),
        ))
        .unwrap();
    let frame = |data: Bytes, method| super::super::peer::QueuedSend {
        data,
        method,
        completion: None,
        _telemetry: None,
        _bulk_permit: None,
    };
    for priority in [false, true] {
        let mut queue = LaneQueue::new(priority);
        queue.push_back(frame(
            wire.clone().into(),
            DeliveryMethod::ReliableUnordered,
        ));
        for _ in 0..32 {
            queue.push_back(frame(
                Bytes::from_static(b"application ping"),
                DeliveryMethod::ReliableOrdered,
            ));
        }
        // A blocked bulk writer must not stop control after the 16-send burst.
        for _ in 0..32 {
            let (ping, independent) = queue.pop(true, false).unwrap();
            assert_eq!(ping.data, Bytes::from_static(b"application ping"));
            assert!(!independent);
        }
        assert!(queue.pop(false, true).unwrap().1);
    }
}
