use super::*;
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, NodeId};
use std::time::Instant;

fn ice_grace(started_at: Instant) -> DisconnectGrace {
    DisconnectGrace {
        started_at,
        origin: GraceOrigin::Ice,
    }
}

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
    t.disconnected_since
        .write()
        .unwrap()
        .insert(node.clone(), ice_grace(Instant::now()));

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
    assert!(
        t.disconnected_since.read().unwrap().is_empty(),
        "disconnected grace entries must be empty after room-level WebRTC cleanup"
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
    assert!(
        !t.disconnected_since.read().unwrap().contains_key(&node),
        "cleanup must clear any disconnected grace entry"
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

/// Buffer-don't-drop fix: a candidate for a node with no `connection_states`
/// reservation at all is now buffered rather than dropped -- see
/// `handle_candidate`'s doc comment for the measured
/// `watchdog_connect_timeout` regression this closes (a trickled Candidate
/// racing ahead of the Offer that would have created the reservation).
/// Superseded the old `late_candidate_for_inactive_node_is_not_buffered`
/// behavior; unbounded growth for genuinely-never-materializing nodes is now
/// prevented by `MAX_PENDING_CANDIDATE_NODES` (see
/// `tests/pending_candidates.rs`) and the sweeper's
/// `pending_candidates_first_seen` TTL sweep instead of an outright drop.
#[tokio::test]
async fn late_candidate_for_a_never_reserved_node_is_now_buffered_not_dropped() {
    let t = make_transport();
    let node = NodeId("never-reserved-peer".to_string());

    t.handle_candidate(node.clone(), "not-json-but-should-be-ignored".to_string())
        .await
        .expect("buffering an unreserved candidate should not error");

    assert_eq!(
        t.pending_candidates.read().await.get(&node),
        Some(&vec!["not-json-but-should-be-ignored".to_string()]),
        "a candidate for a never-reserved node must now be buffered, not dropped"
    );
    assert!(
        t.pending_candidates_first_seen
            .read()
            .await
            .contains_key(&node),
        "buffering a brand-new node's first candidate must record its first-seen timestamp \
         for the sweeper's unreserved-node TTL sweep"
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

#[tokio::test]
async fn cleanup_resets_signaling_session_only_when_transport_stays_isolated() {
    use async_trait::async_trait;
    use mistlib_core::error::Result as MistResult;
    use mistlib_core::signaling::{MessageContent, Signaler};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct ResetCountingSignaler(AtomicUsize);

    #[async_trait]
    impl Signaler for ResetCountingSignaler {
        async fn send_signaling(&self, _to: &NodeId, _msg: MessageContent) -> MistResult<()> {
            Ok(())
        }

        async fn reset_session(&self) -> MistResult<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn close(&self) -> MistResult<()> {
            Ok(())
        }
    }

    let signaler = Arc::new(ResetCountingSignaler(AtomicUsize::new(0)));
    let t = WebRtcTransport::new(signaler.clone(), NodeId("local".to_string()));
    let node = NodeId("peer".to_string());
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connected);

    t.cleanup_session(&node, false).await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    assert_eq!(signaler.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cleanup_does_not_reset_signaling_session_when_another_peer_is_active() {
    use async_trait::async_trait;
    use mistlib_core::error::Result as MistResult;
    use mistlib_core::signaling::{MessageContent, Signaler};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct ResetCountingSignaler(AtomicUsize);

    #[async_trait]
    impl Signaler for ResetCountingSignaler {
        async fn send_signaling(&self, _to: &NodeId, _msg: MessageContent) -> MistResult<()> {
            Ok(())
        }

        async fn reset_session(&self) -> MistResult<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn close(&self) -> MistResult<()> {
            Ok(())
        }
    }

    let signaler = Arc::new(ResetCountingSignaler(AtomicUsize::new(0)));
    let t = WebRtcTransport::new(signaler.clone(), NodeId("local".to_string()));
    let disconnected = NodeId("disconnected".to_string());
    t.connection_states
        .write()
        .unwrap()
        .insert(disconnected.clone(), ConnectionState::Connected);
    t.connection_states.write().unwrap().insert(
        NodeId("still-active".to_string()),
        ConnectionState::Connected,
    );

    t.cleanup_session(&disconnected, false).await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    assert_eq!(signaler.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn disconnected_grace_marks_reconnecting_and_recovers_without_cleanup() {
    let t = make_transport();
    let node = NodeId("grace-recover-peer".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("peer connection should be created for grace test");

    t.peers.write().await.insert(node.clone(), peer);
    t.connection_attempt_ids
        .write()
        .unwrap()
        .insert(node.clone(), 99);
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connected);

    let handles = t.peer_handles();
    let (reserved, freshly_started) = handles.mark_disconnected_grace(&node);
    assert!(reserved);
    assert!(freshly_started);

    assert_eq!(t.get_connection_state(&node), ConnectionState::Reconnecting);
    assert!(t.peers.read().await.contains_key(&node));
    assert!(t.connection_attempt_ids.read().unwrap().contains_key(&node));
    assert!(t.disconnected_since.read().unwrap().contains_key(&node));

    assert!(handles.mark_connection_state(&node, ConnectionState::Connected));

    assert_eq!(t.get_connection_state(&node), ConnectionState::Connected);
    assert!(t.peers.read().await.contains_key(&node));
    assert!(t.disconnected_since.read().unwrap().is_empty());
    assert!(
        t.last_disconnect_at.read().unwrap().is_empty(),
        "recovering during grace must not start reconnect cooldown"
    );
}

#[tokio::test]
async fn disconnected_grace_expiry_is_cleaned_by_sweeper() {
    use super::DISCONNECTED_GRACE_MS;
    use tokio::time::{sleep, timeout, Duration};

    let t = make_transport();
    let node = NodeId("grace-expired-peer".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("peer connection should be created for grace expiry test");

    t.peers.write().await.insert(node.clone(), peer);
    t.pending_candidates
        .write()
        .await
        .insert(node.clone(), vec!["candidate".to_string()]);
    t.connection_attempt_ids
        .write()
        .unwrap()
        .insert(node.clone(), 101);
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Reconnecting);
    t.disconnected_since.write().unwrap().insert(
        node.clone(),
        ice_grace(Instant::now() - Duration::from_millis(DISCONNECTED_GRACE_MS + 1)),
    );

    t.ensure_session_sweeper();

    timeout(Duration::from_secs(2), async {
        loop {
            if !t.peers.read().await.contains_key(&node) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sweeper should clean expired disconnected grace session");
    t.stop_session_sweeper();

    assert_eq!(t.get_connection_state(&node), ConnectionState::Disconnected);
    assert!(!t.pending_candidates.read().await.contains_key(&node));
    assert!(!t.connection_attempt_ids.read().unwrap().contains_key(&node));
    assert!(!t.disconnected_since.read().unwrap().contains_key(&node));
    assert!(t.last_disconnect_at.read().unwrap().contains_key(&node));
}

#[tokio::test]
async fn suspect_disconnected_moves_a_connected_peer_into_a_liveness_suspect_grace() {
    let t = make_transport();
    let node = NodeId("suspect-peer".to_string());

    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connected);

    let handles = t.peer_handles();
    assert!(handles.mark_suspect_disconnected(&node));

    assert_eq!(t.get_connection_state(&node), ConnectionState::Reconnecting);
    let disconnected = t.disconnected_since.read().unwrap();
    let grace = disconnected
        .get(&node)
        .expect("liveness-suspect grace should be recorded");
    assert_eq!(grace.origin, GraceOrigin::LivenessSuspect);
}

#[tokio::test]
async fn suspect_disconnected_is_a_noop_for_a_peer_that_is_not_connected() {
    let t = make_transport();
    let node = NodeId("connecting-peer".to_string());

    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connecting);

    let handles = t.peer_handles();
    assert!(
        !handles.mark_suspect_disconnected(&node),
        "a peer that is not yet Connected must not be pulled into a suspect grace"
    );
    assert_eq!(t.get_connection_state(&node), ConnectionState::Connecting);
    assert!(t.disconnected_since.read().unwrap().is_empty());
}

#[tokio::test]
async fn suspect_disconnected_does_not_restart_an_already_running_ice_grace() {
    let t = make_transport();
    let node = NodeId("already-in-ice-grace-peer".to_string());

    // Reconnecting via an ICE-originated grace already in progress.
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Reconnecting);
    let handles = t.peer_handles();
    handles.mark_disconnected_grace(&node);

    // A liveness suspicion arriving while ICE's own grace is already running
    // must not touch it: mark_suspect_disconnected only ever acts on a peer
    // that is currently Connected.
    assert!(!handles.mark_suspect_disconnected(&node));
    let disconnected = t.disconnected_since.read().unwrap();
    assert_eq!(disconnected.get(&node).unwrap().origin, GraceOrigin::Ice);
}

#[tokio::test]
async fn clear_suspect_cancels_a_liveness_suspect_grace_and_restores_connected() {
    let t = make_transport();
    let node = NodeId("clear-suspect-peer".to_string());

    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connected);
    let handles = t.peer_handles();
    assert!(handles.mark_suspect_disconnected(&node));
    assert_eq!(t.get_connection_state(&node), ConnectionState::Reconnecting);

    assert!(handles.clear_suspect(&node));

    assert_eq!(t.get_connection_state(&node), ConnectionState::Connected);
    assert!(
        !t.disconnected_since.read().unwrap().contains_key(&node),
        "clearing the suspect grace must drop its disconnected_since entry"
    );
}

/// The critical invariant: a grace period started by an ICE `Disconnected`
/// event must survive `ClearSuspect`. If it didn't, a false-positive liveness
/// suspicion (transient PONG loss on an otherwise-fine connection) could race
/// with a real ICE recovery and rip out a perfectly healthy peer -- or worse,
/// paper over a real ICE-level grace that only ICE's own recovery signal
/// should be allowed to end.
#[tokio::test]
async fn clear_suspect_does_not_touch_an_ice_originated_grace() {
    let t = make_transport();
    let node = NodeId("ice-grace-peer".to_string());

    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connected);
    let handles = t.peer_handles();
    let (reserved, freshly_started) = handles.mark_disconnected_grace(&node);
    assert!(reserved);
    assert!(freshly_started);
    assert_eq!(t.get_connection_state(&node), ConnectionState::Reconnecting);

    assert!(
        !handles.clear_suspect(&node),
        "ClearSuspect must refuse to cancel a grace it did not start"
    );

    // The ICE-originated grace must still be fully in effect afterwards.
    assert_eq!(t.get_connection_state(&node), ConnectionState::Reconnecting);
    let disconnected = t.disconnected_since.read().unwrap();
    assert_eq!(disconnected.get(&node).unwrap().origin, GraceOrigin::Ice);
}

#[tokio::test]
async fn transport_suspect_and_clear_suspect_round_trip_through_the_transport_trait() {
    let t = make_transport();
    let node = NodeId("transport-level-peer".to_string());

    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connected);

    t.suspect_disconnected(&node)
        .await
        .expect("suspect_disconnected should not error");
    assert_eq!(t.get_connection_state(&node), ConnectionState::Reconnecting);

    t.clear_suspect(&node)
        .await
        .expect("clear_suspect should not error");
    assert_eq!(t.get_connection_state(&node), ConnectionState::Connected);
}

#[tokio::test]
async fn zombie_pc_connected_without_reliable_channel_is_cleaned_by_sweeper() {
    use super::DATA_CHANNEL_OPEN_TIMEOUT_MS;
    use tokio::time::{sleep, timeout, Duration};

    let t = make_transport();
    let node = NodeId("zombie-no-reliable-dc".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("peer connection should be created for zombie cleanup test");

    t.peers.write().await.insert(node.clone(), peer);
    t.pending_candidates
        .write()
        .await
        .insert(node.clone(), vec!["candidate".to_string()]);
    t.connection_attempt_ids
        .write()
        .unwrap()
        .insert(node.clone(), 202);
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connecting);
    t.pc_connected_at.write().unwrap().insert(
        node.clone(),
        Instant::now() - Duration::from_millis(DATA_CHANNEL_OPEN_TIMEOUT_MS + 1),
    );

    t.ensure_session_sweeper();

    timeout(Duration::from_secs(2), async {
        loop {
            if !t.peers.read().await.contains_key(&node) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sweeper should clean pc-connected session with no ReliableOrdered DataChannel");
    t.stop_session_sweeper();

    assert_eq!(t.get_connection_state(&node), ConnectionState::Disconnected);
    assert!(!t.pending_candidates.read().await.contains_key(&node));
    assert!(!t.connection_attempt_ids.read().unwrap().contains_key(&node));
    assert!(!t.pc_connected_at.read().unwrap().contains_key(&node));
    assert!(t.last_disconnect_at.read().unwrap().contains_key(&node));
}

/// Regression test for a `pc_connected_at` staleness gap adjacent to the
/// ICE-restart Connected-state recovery fix (`recover_connected_from_grace`
/// in `peer.rs`): `RTCPeerConnectionState::Connected`'s handler re-arms
/// `pc_connected_at` on *every* transition to `Connected`, including a
/// restart recovery where the ReliableOrdered data channel survived the
/// whole episode without ever closing. Because that channel's `on_open`
/// handler already fired once (at the original connect) and webrtc-rs never
/// fires it again, nothing used to clear this second `pc_connected_at`
/// entry -- it would sit in the map forever, long past
/// `DATA_CHANNEL_OPEN_TIMEOUT_MS`. The sweeper must disarm it itself as soon
/// as it observes the required channel is open, exactly like `on_open`
/// would have; otherwise a perfectly healthy session is one transient
/// non-`Open` `ready_state()` read away from being force-closed as a
/// false-positive "zombie".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sweeper_disarms_stale_pc_connected_at_once_channel_is_confirmed_open() {
    use super::disconnect::{make_connected_pair, wait_for_state};
    use super::DATA_CHANNEL_OPEN_TIMEOUT_MS;

    let (ta, _tb, _id_a, id_b) = make_connected_pair();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );

    // Simulate the leftover from an ICE-restart recovery: `pc_connected_at`
    // re-armed by the `Connected` handler, already long past the DC-open
    // timeout, even though the ReliableOrdered channel has stayed open and
    // healthy the entire time.
    ta.pc_connected_at.write().unwrap().insert(
        id_b.clone(),
        std::time::Instant::now()
            - std::time::Duration::from_millis(DATA_CHANNEL_OPEN_TIMEOUT_MS + 1),
    );

    ta.ensure_session_sweeper();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    ta.stop_session_sweeper();

    assert!(
        !ta.pc_connected_at.read().unwrap().contains_key(&id_b),
        "sweeper must clear a stale pc_connected_at once the required DC is confirmed open"
    );
    assert_eq!(
        ta.get_connection_state(&id_b),
        ConnectionState::Connected,
        "a healthy session with an open DC must not be force-closed by a stale zombie timer"
    );
    assert!(ta.peers.read().await.contains_key(&id_b));
}

/// Regression test for a permanent-"Node not found" variant of the
/// stale-teardown race: unlike the `Failed`/`Closed` and data-channel-close
/// callbacks (already guarded by `remove_peer_if_current`), the connect
/// watchdog and the periodic sweeper used to call the *unconditional*
/// `cleanup_session_with_reason` -- keyed only by `NodeId`, with no check
/// that the peer they read moments earlier is still the one currently
/// registered. If a fresh reconnect for the same `NodeId` installs a new,
/// healthy peer (marking `connection_states` `Connected` via its own DC-open
/// handler) in the window between the watchdog/sweeper's stale read and its
/// cleanup call, the old, unconditional cleanup would delete that new peer's
/// `self.peers` entry while leaving `connection_states` untouched -- since
/// nothing else in this module re-populates `connection_states` without
/// first creating a fresh peer, the node would then look permanently
/// "Connected" to the overlay (which keeps addressing sends to it) while
/// every `wt.send()` fails forever with "Node not found", and with no
/// disconnect/state-change log to explain why.
///
/// `cleanup_session_if_current` (used by the watchdog and sweeper since this
/// fix) closes this the same way `remove_peer_if_current` already does for
/// the other two teardown paths: a stale caller's cleanup for a peer that no
/// longer owns the `NodeId` key must be a complete no-op, not just a
/// no-op-on-`self.peers`.
#[tokio::test]
async fn cleanup_session_if_current_ignores_a_stale_peer_already_superseded_by_a_reconnect() {
    use std::sync::Arc;

    let t = make_transport();
    let node = NodeId("stale-watchdog-vs-fresh-reconnect".to_string());

    let old_peer = t
        .create_pc(node.clone())
        .await
        .expect("old peer should be created");
    let new_peer = t
        .create_pc(node.clone())
        .await
        .expect("new peer should be created");
    assert!(
        !Arc::ptr_eq(&old_peer, &new_peer),
        "test setup should produce two distinct Peer instances"
    );

    // Old attempt's bookkeeping, as it would look right before its
    // watchdog/sweeper decided (based on a since-stale read) to clean it up.
    t.peers.write().await.insert(node.clone(), old_peer.clone());
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connecting);
    t.connection_attempt_ids
        .write()
        .unwrap()
        .insert(node.clone(), 1);

    // A fresh reconnect races in and wins: it replaces the peer and marks
    // the node fully Connected -- mirroring what `handle_offer`/
    // `connect_inner` + the ReliableOrdered DC's `on_open` handler would
    // really do.
    t.peers.write().await.insert(node.clone(), new_peer.clone());
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connected);
    t.connection_attempt_ids
        .write()
        .unwrap()
        .insert(node.clone(), 2);

    // The old attempt's watchdog/sweeper cleanup finally runs, guarded on
    // its own (now-stale) peer snapshot. It must be a complete no-op.
    let old_expected = Arc::downgrade(&old_peer);
    t.cleanup_session_if_current(&node, &old_expected, true, "watchdog_connect_timeout")
        .await;

    assert!(
        matches!(t.peers.read().await.get(&node), Some(p) if Arc::ptr_eq(p, &new_peer)),
        "a stale watchdog/sweeper cleanup must not remove the new peer's registration"
    );
    assert_eq!(
        t.get_connection_state(&node),
        ConnectionState::Connected,
        "a stale cleanup must not touch connection_states once a fresh reconnect owns the node"
    );
    assert_eq!(
        t.connection_attempt_ids.read().unwrap().get(&node).copied(),
        Some(2),
        "a stale cleanup must not remove the current attempt id"
    );

    // The *new* attempt's own cleanup, by contrast, must still work.
    let new_expected = Arc::downgrade(&new_peer);
    t.cleanup_session_if_current(&node, &new_expected, true, "watchdog_connect_timeout")
        .await;

    assert!(!t.peers.read().await.contains_key(&node));
    assert_eq!(t.get_connection_state(&node), ConnectionState::Disconnected);
}
