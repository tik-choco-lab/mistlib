use super::*;
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, NodeId};
use std::time::Instant;

#[tokio::test]
async fn close_all_peer_connections_clears_all_transport_state() {
    let t = make_transport();
    let node = NodeId("peer-to-close".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("peer connection should be created for cleanup test");

    t.peers.write().await.insert(node.clone(), peer);
    t.pending_candidates
        .write()
        .await
        .insert(node.clone(), vec!["late-candidate".to_string()]);
    t.connection_attempt_ids
        .write()
        .unwrap()
        .insert(node.clone(), 42);
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connecting);
    t.last_disconnect_at
        .write()
        .unwrap()
        .insert(node.clone(), Instant::now());

    t.close_all_peer_connections().await;

    assert!(
        t.peers.read().await.is_empty(),
        "peers must be empty after room-level WebRTC cleanup"
    );
    assert!(
        t.pending_candidates.read().await.is_empty(),
        "pending ICE candidates must be empty after room-level WebRTC cleanup"
    );
    assert!(
        t.connection_attempt_ids.read().unwrap().is_empty(),
        "connection attempt ids must be empty after room-level WebRTC cleanup"
    );
    assert!(
        t.connection_states.read().unwrap().is_empty(),
        "connection states must be empty after room-level WebRTC cleanup"
    );
    assert!(
        t.last_disconnect_at.read().unwrap().is_empty(),
        "disconnect cooldown entries must be empty after room-level WebRTC cleanup"
    );
}

#[tokio::test]
async fn force_failed_cleanup_removes_peer_state_and_pending_candidates() {
    let t = make_transport();
    let node = NodeId("force-failed-peer".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("peer connection should be created for cleanup test");

    t.peers.write().await.insert(node.clone(), peer);
    t.pending_candidates
        .write()
        .await
        .insert(node.clone(), vec!["late-candidate".to_string()]);
    t.connection_attempt_ids
        .write()
        .unwrap()
        .insert(node.clone(), 7);
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connecting);

    t.cleanup_session(&node, true).await;

    assert!(
        !t.peers.read().await.contains_key(&node),
        "force-failed cleanup must remove the peer entry"
    );
    assert!(
        !t.pending_candidates.read().await.contains_key(&node),
        "force-failed cleanup must remove pending ICE candidates"
    );
    assert!(
        !t.connection_attempt_ids.read().unwrap().contains_key(&node),
        "force-failed cleanup must remove the attempt id"
    );
    assert_eq!(
        t.get_connection_state(&node),
        ConnectionState::Disconnected,
        "force-failed cleanup must not leave a Failed state behind"
    );
    assert!(
        t.last_disconnect_at.read().unwrap().contains_key(&node),
        "force-failed cleanup should retain a disconnect cooldown entry"
    );
}

#[tokio::test]
async fn cleanup_unknown_node_does_not_create_disconnect_cooldown_entry() {
    let t = make_transport();
    let node = NodeId("never-seen-peer".to_string());

    t.cleanup_session(&node, false).await;

    assert!(
        !t.last_disconnect_at.read().unwrap().contains_key(&node),
        "cleanup for an unknown node must not grow the reconnect cooldown map"
    );
}

#[tokio::test]
async fn late_candidate_for_inactive_node_is_not_buffered() {
    let t = make_transport();
    let node = NodeId("inactive-peer".to_string());

    t.handle_candidate(node.clone(), "not-json-but-should-be-ignored".to_string())
        .await
        .expect("inactive candidate should be ignored before parsing");

    assert!(
        !t.pending_candidates.read().await.contains_key(&node),
        "late ICE candidates for inactive nodes must not accumulate"
    );
}

#[tokio::test]
async fn candidate_for_active_node_is_buffered_until_peer_can_accept_it() {
    let t = make_transport();
    let node = NodeId("active-peer".to_string());
    let candidate = "candidate-json-is-not-parsed-until-a-peer-is-ready".to_string();

    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connecting);

    t.handle_candidate(node.clone(), candidate.clone())
        .await
        .expect("active candidate should be buffered");

    let pending = t.pending_candidates.read().await;
    assert_eq!(
        pending.get(&node),
        Some(&vec![candidate]),
        "active nodes should still buffer candidates until the peer can accept them"
    );
}
