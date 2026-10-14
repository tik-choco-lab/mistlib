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
