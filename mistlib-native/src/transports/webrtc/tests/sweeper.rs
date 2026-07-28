use super::disconnect::{make_connected_pair, wait_for_state};
use crate::transports::webrtc::sweeper::{
    decide_grace_expiry, reservation_reap_allowed, GraceExpiryDecision,
    CONNECTING_RESERVATION_REAP_GATE_MS,
};
use crate::transports::webrtc::{DisconnectGrace, GraceOrigin};
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::time::{Duration, Instant};
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

const GRACE_MS: u64 = 50;

fn grace(origin: GraceOrigin, elapsed: Duration) -> DisconnectGrace {
    DisconnectGrace {
        started_at: Instant::now() - elapsed,
        origin,
    }
}

// --- Pure `decide_grace_expiry` unit tests -------------------------------
//
// These exhaustively cover the decision table described at the sweeper's
// call site: only a `LivenessSuspect`-origin grace that has both a `Connected`
// pc *and* an open ReliableOrdered data channel is ever suppressed as a false
// positive. Everything else reaps exactly as it did before this fix.

#[test]
fn no_grace_is_always_wait() {
    assert_eq!(
        decide_grace_expiry(None, RTCPeerConnectionState::Connected, true, GRACE_MS),
        GraceExpiryDecision::Wait
    );
    assert_eq!(
        decide_grace_expiry(None, RTCPeerConnectionState::Disconnected, false, GRACE_MS),
        GraceExpiryDecision::Wait
    );
}

#[test]
fn grace_not_yet_elapsed_is_wait_regardless_of_origin_or_health() {
    let not_yet = Duration::from_millis(GRACE_MS / 2);
    assert_eq!(
        decide_grace_expiry(
            Some(grace(GraceOrigin::LivenessSuspect, not_yet)),
            RTCPeerConnectionState::Connected,
            true,
            GRACE_MS
        ),
        GraceExpiryDecision::Wait
    );
    assert_eq!(
        decide_grace_expiry(
            Some(grace(GraceOrigin::Ice, not_yet)),
            RTCPeerConnectionState::Disconnected,
            false,
            GRACE_MS
        ),
        GraceExpiryDecision::Wait
    );
}

#[test]
fn liveness_suspect_expired_with_healthy_pc_and_open_dc_is_recovered_as_false_positive() {
    let expired = Duration::from_millis(GRACE_MS + 1);
    assert_eq!(
        decide_grace_expiry(
            Some(grace(GraceOrigin::LivenessSuspect, expired)),
            RTCPeerConnectionState::Connected,
            true,
            GRACE_MS
        ),
        GraceExpiryDecision::RecoverFalsePositive,
        "a missed-PONG-only grace against an actually-healthy pc+DC must not be reaped"
    );
}

#[test]
fn liveness_suspect_expired_without_open_dc_is_still_reaped() {
    let expired = Duration::from_millis(GRACE_MS + 1);
    assert_eq!(
        decide_grace_expiry(
            Some(grace(GraceOrigin::LivenessSuspect, expired)),
            RTCPeerConnectionState::Connected,
            false,
            GRACE_MS
        ),
        GraceExpiryDecision::Reap,
        "pc reporting Connected is not enough on its own -- the required data channel must also be open"
    );
}

#[test]
fn liveness_suspect_expired_with_unhealthy_pc_is_still_reaped() {
    let expired = Duration::from_millis(GRACE_MS + 1);
    for pc_state in [
        RTCPeerConnectionState::Disconnected,
        RTCPeerConnectionState::Failed,
        RTCPeerConnectionState::Closed,
        RTCPeerConnectionState::Connecting,
        RTCPeerConnectionState::New,
    ] {
        assert_eq!(
            decide_grace_expiry(
                Some(grace(GraceOrigin::LivenessSuspect, expired)),
                pc_state,
                true,
                GRACE_MS
            ),
            GraceExpiryDecision::Reap,
            "a LivenessSuspect grace must still reap when the real pc state ({:?}) is not Connected",
            pc_state
        );
    }
}

#[test]
fn ice_origin_expired_grace_always_reaps_even_if_pc_looks_connected() {
    let expired = Duration::from_millis(GRACE_MS + 1);
    // An Ice-origin grace is never second-guessed against the live pc state --
    // ICE itself is what reported trouble, so there is nothing to re-validate.
    assert_eq!(
        decide_grace_expiry(
            Some(grace(GraceOrigin::Ice, expired)),
            RTCPeerConnectionState::Connected,
            true,
            GRACE_MS
        ),
        GraceExpiryDecision::Reap,
        "Ice-origin grace expiry behavior must be unchanged by the LivenessSuspect fix"
    );
    assert_eq!(
        decide_grace_expiry(
            Some(grace(GraceOrigin::Ice, expired)),
            RTCPeerConnectionState::Disconnected,
            false,
            GRACE_MS
        ),
        GraceExpiryDecision::Reap
    );
}

// --- Pure `reservation_reap_allowed` unit tests (sweeper livelock fix) ---

#[test]
fn no_reservation_timestamp_is_reaped_immediately() {
    // A missing entry is itself an anomaly (every current reservation path
    // always records one) -- treated as reapable on sight, matching this
    // branch's pre-fix behavior for that case.
    assert!(reservation_reap_allowed(None, 1_000));
}

#[test]
fn a_reservation_younger_than_the_gate_is_not_reaped() {
    assert!(!reservation_reap_allowed(Some(Instant::now()), 1_000));
}

#[test]
fn a_reservation_older_than_the_gate_is_reaped() {
    let old = Instant::now() - Duration::from_millis(1_001);
    assert!(reservation_reap_allowed(Some(old), 1_000));
}

#[test]
fn boundary_elapsed_exactly_at_the_gate_is_reaped() {
    // Inclusive boundary, matching `takeover_allowed`'s own
    // strict-`<`-blocks convention.
    let boundary = Instant::now() - Duration::from_millis(1_000);
    assert!(reservation_reap_allowed(Some(boundary), 1_000));
}

// --- Sweeper integration tests -------------------------------------------

/// The core regression scenario from the bug report: a real, healthy
/// connection (both peers reach `Connected` with an open ReliableOrdered
/// data channel -- exactly what an SSH tunnel would ride) gets a
/// `LivenessSuspect`-origin grace injected as if 5 missed PONGs had just
/// fired on the best-effort ping channel, already past `DISCONNECTED_GRACE_MS`.
/// The sweeper must NOT tear the session down: it must clear the grace and
/// restore `Connected`, leaving the peer (and its data channel) alive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn liveness_suspect_grace_with_healthy_connection_is_recovered_not_reaped() {
    use crate::transports::webrtc::DISCONNECTED_GRACE_MS;

    let (ta, _tb, _id_a, id_b) = make_connected_pair();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );

    let peer = ta
        .peers
        .read()
        .await
        .get(&id_b)
        .cloned()
        .expect("A should have a live peer for B");
    assert!(
        {
            let channels = peer.channels.read().await;
            channels
                .get(&DeliveryMethod::ReliableOrdered)
                .is_some_and(|dc| dc.ready_state() == RTCDataChannelState::Open)
        },
        "test setup: ReliableOrdered DC must be open before injecting the false-positive grace"
    );

    // Simulate `mark_suspect_disconnected` having already fired and its grace
    // already past `DISCONNECTED_GRACE_MS`, exactly as it would look right
    // before the (buggy, pre-fix) sweeper would have reaped it.
    ta.connection_states
        .write()
        .unwrap()
        .insert(id_b.clone(), ConnectionState::Reconnecting);
    ta.disconnected_since.write().unwrap().insert(
        id_b.clone(),
        DisconnectGrace {
            started_at: Instant::now() - Duration::from_millis(DISCONNECTED_GRACE_MS + 1),
            origin: GraceOrigin::LivenessSuspect,
        },
    );

    ta.ensure_session_sweeper();
    // Give the sweeper multiple ticks (SWEEPER_INTERVAL_MS is 10ms under
    // test) to run its decision at least once.
    tokio::time::sleep(Duration::from_millis(200)).await;
    ta.stop_session_sweeper();

    assert_eq!(
        ta.get_connection_state(&id_b),
        ConnectionState::Connected,
        "a false-positive liveness-suspect grace on an actually-healthy connection must be cleared, not left Reconnecting/torn down"
    );
    assert!(
        !ta.disconnected_since.read().unwrap().contains_key(&id_b),
        "the suppressed grace entry must be cleared"
    );
    assert!(
        ta.peers.read().await.contains_key(&id_b),
        "the healthy peer must survive -- this is the SSH-tunnel-drop regression"
    );

    // The data channel must still be genuinely usable afterward, not just
    // nominally present.
    ta.send(
        &id_b,
        bytes::Bytes::from_static(b"still-alive-after-false-positive-grace"),
        DeliveryMethod::ReliableOrdered,
    )
    .await
    .expect("data channel must still be usable after the false positive is suppressed");
}

/// Counterpart to the recovery test: a `LivenessSuspect`-origin grace against
/// a peer that is genuinely NOT healthy (never got past `New`/no data
/// channel open at all) must still be reaped once the grace period expires --
/// the false-positive suppression must not turn into a blanket amnesty for
/// every liveness-suspect grace.
#[tokio::test]
async fn liveness_suspect_grace_with_unhealthy_pc_is_still_reaped() {
    use crate::transports::webrtc::DISCONNECTED_GRACE_MS;

    let t = super::make_transport();
    let node = NodeId("liveness-suspect-unhealthy-peer".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("peer connection should be created for this test");

    t.peers.write().await.insert(node.clone(), peer);
    t.connection_attempt_ids
        .write()
        .unwrap()
        .insert(node.clone(), 1);
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Reconnecting);
    t.disconnected_since.write().unwrap().insert(
        node.clone(),
        DisconnectGrace {
            started_at: Instant::now() - Duration::from_millis(DISCONNECTED_GRACE_MS + 1),
            origin: GraceOrigin::LivenessSuspect,
        },
    );

    t.ensure_session_sweeper();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !t.peers.read().await.contains_key(&node) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sweeper should reap a LivenessSuspect grace against a genuinely unhealthy peer");
    t.stop_session_sweeper();

    assert_eq!(t.get_connection_state(&node), ConnectionState::Disconnected);
    assert!(!t.disconnected_since.read().unwrap().contains_key(&node));
}

/// Explicit ICE-origin counterpart alongside the two LivenessSuspect tests
/// above: an ICE-origin grace on a peer that superficially "looks" reachable
/// (fresh pc, no real connection ever established) must reap exactly as
/// before -- this exercises the same sweeper code path the LivenessSuspect
/// fix touched, just with the other origin, so a regression in either
/// direction shows up here.
#[tokio::test]
async fn ice_origin_grace_expiry_is_unaffected_by_the_liveness_suspect_fix() {
    use crate::transports::webrtc::DISCONNECTED_GRACE_MS;

    let t = super::make_transport();
    let node = NodeId("ice-origin-unaffected-peer".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("peer connection should be created for this test");

    t.peers.write().await.insert(node.clone(), peer);
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Reconnecting);
    t.disconnected_since.write().unwrap().insert(
        node.clone(),
        DisconnectGrace {
            started_at: Instant::now() - Duration::from_millis(DISCONNECTED_GRACE_MS + 1),
            origin: GraceOrigin::Ice,
        },
    );

    t.ensure_session_sweeper();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !t.peers.read().await.contains_key(&node) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("an Ice-origin grace must still reap on expiry");
    t.stop_session_sweeper();

    assert_eq!(t.get_connection_state(&node), ConnectionState::Disconnected);
    assert!(!t.disconnected_since.read().unwrap().contains_key(&node));
}

// --- Sweeper livelock fix: no-peer-registered branch's reservation gate --

/// The core regression this fix closes: a `Connecting` reservation with no
/// `self.peers` entry yet can legitimately still be queued on
/// `acquire_handshake_permit` (no timeout of its own). The sweeper must not
/// reap it on the very first tick that observes it -- doing so used to make
/// the dial silently no-op once its permit finally arrived
/// (`connect_inner`'s `has_active_session` check would see the reservation
/// already gone), with DNVE3 reissuing `Connect` forever.
#[tokio::test]
async fn sweeper_does_not_reap_a_fresh_connecting_reservation_with_no_peer() {
    let t = super::make_transport();
    let node = NodeId("queued-on-semaphore-peer".to_string());

    // Deliberately no `self.peers` entry -- reproduces the queued-on-the-
    // handshake-semaphore window between `connect()`'s reservation and
    // `connect_inner`'s `acquire_handshake_permit` resolving.
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connecting);
    t.connecting_reserved_at
        .write()
        .unwrap()
        .insert(node.clone(), Instant::now());

    t.ensure_session_sweeper();
    tokio::time::sleep(Duration::from_millis(
        CONNECTING_RESERVATION_REAP_GATE_MS / 2,
    ))
    .await;
    t.stop_session_sweeper();

    assert_eq!(
        t.get_connection_state(&node),
        ConnectionState::Connecting,
        "a reservation still younger than the gate must survive the sweeper untouched"
    );
    assert!(
        t.connecting_reserved_at.read().unwrap().contains_key(&node),
        "the reservation timestamp itself must also survive"
    );
}

/// Counterpart: once a bare reservation is older than the gate, the sweeper
/// must reap it (and log that this normally-silent branch fired) rather than
/// leave it abandoned forever.
#[tokio::test]
async fn sweeper_reaps_a_stale_connecting_reservation_with_no_peer_past_the_gate() {
    let t = super::make_transport();
    let node = NodeId("abandoned-reservation-peer".to_string());

    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connecting);
    t.connecting_reserved_at.write().unwrap().insert(
        node.clone(),
        Instant::now() - Duration::from_millis(CONNECTING_RESERVATION_REAP_GATE_MS + 1),
    );

    t.ensure_session_sweeper();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if t.get_connection_state(&node) == ConnectionState::Disconnected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sweeper should reap a Connecting reservation older than the gate");
    t.stop_session_sweeper();

    assert!(
        !t.connecting_reserved_at.read().unwrap().contains_key(&node),
        "the stale reservation timestamp must be cleared alongside the reap"
    );
}
