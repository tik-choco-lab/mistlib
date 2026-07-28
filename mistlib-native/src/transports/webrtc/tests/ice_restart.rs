use super::disconnect::{make_connected_pair, wait_for_state};
use crate::transports::webrtc::{
    ice_restart_allowed, is_ice_restart_initiator, is_restart_request, Peer, RESTART_REQUEST_MARKER,
};
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::sync::Arc as StdArc;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

#[test]
fn is_ice_restart_initiator_uses_lower_id_wins_direction() {
    let a = NodeId("aaa".to_string());
    let b = NodeId("bbb".to_string());
    assert!(
        is_ice_restart_initiator(&a, &b),
        "the lower node ID must be the ICE-restart initiator"
    );
    assert!(
        !is_ice_restart_initiator(&b, &a),
        "the higher node ID must not be the ICE-restart initiator"
    );
    assert!(
        !is_ice_restart_initiator(&a, &a),
        "a node is never its own initiator"
    );
}

/// Polls (rather than sleeping a fixed duration) `peer`'s `method` DataChannel
/// until it reports `Open`. Needed because the aggregate
/// `RTCPeerConnectionState` (what `wait_for_state`/`get_connection_state`
/// observe) can flip to `Connected` slightly *before* a DataChannel finishes
/// reopening its SCTP stream after an ICE restart -- checking the DC itself
/// is the only way to know `Transport::send` won't race a not-actually-open
/// channel (see the "not open (state: Connecting)" failure this replaced).
async fn wait_for_dc_open(peer: &Peer, method: DeliveryMethod, timeout_ms: u64) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let is_open = {
            let channels = peer.channels.read().await;
            channels
                .get(&method)
                .is_some_and(|dc| dc.ready_state() == RTCDataChannelState::Open)
        };
        if is_open {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// `try_ice_restart` renegotiates ICE on the SAME `RTCPeerConnection` (an
/// offer with `ice_restart: true`, applied by the peer in place via the
/// existing Stable-signaling-state renegotiation path in `signaling.rs` --
/// the same mechanism `renegotiation_offer_on_existing_peer_is_applied_in_place`
/// in `signaling.rs` already exercises for a track-add renegotiation). It
/// must not tear down and recreate either side's `Peer`, and the data
/// channel must remain usable afterward.
///
/// This calls `try_ice_restart` directly (bypassing the
/// `is_ice_restart_initiator` gate that guards the real ICE-`Disconnected`
/// trigger) so the test exercises the restart mechanism itself without
/// depending on actually forcing an ICE disconnection, which would be far
/// more timing-sensitive to set up reliably. It also polls for the reliable
/// DataChannel's own `Open` state (`wait_for_dc_open`) instead of racing on
/// the aggregate connection state, and gives real ICE renegotiation a
/// generous budget -- on restricted networks the stack can fail to bind several
/// of its own advertised interfaces during candidate gathering (observed via
/// `RUST_LOG=debug`), which can slow an ICE restart's reconvergence well
/// beyond the ~1s common case.
///
/// Known flaky, same as `disconnect.rs`'s tests: a real ICE renegotiation
/// occasionally exceeds even this generous budget. Treat a
/// failure here the same way -- rerun in isolation before treating it as a
/// regression.
///
/// multi_thread required: see the reasoning in disconnect.rs / signaling.rs --
/// A/B are independent peers and must not share an OS thread for realistic
/// scheduling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn try_ice_restart_keeps_peer_and_data_channel_alive() {
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

    let peer_a_before = ta
        .peers
        .read()
        .await
        .get(&id_b)
        .cloned()
        .expect("A should have a live peer for B");
    let peer_b_before = tb
        .peers
        .read()
        .await
        .get(&id_a)
        .cloned()
        .expect("B should have a live peer for A");

    // A is the initiator toward B for this fixed pair ("peer-a" < "peer-b"),
    // matching the direction `try_ice_restart`'s real trigger would use.
    assert!(is_ice_restart_initiator(&id_a, &id_b));

    ta.peer_handles().try_ice_restart(&id_b).await;

    assert!(
        wait_for_dc_open(&peer_a_before, DeliveryMethod::ReliableOrdered, 25_000).await,
        "A's reliable data channel did not reopen after the ICE restart"
    );
    assert!(
        wait_for_dc_open(&peer_b_before, DeliveryMethod::ReliableOrdered, 25_000).await,
        "B's reliable data channel did not reopen after the ICE restart"
    );

    // Peer identity must be unchanged throughout: `apply_offer`'s
    // Stable-signaling-state branch (taken because both sides already have a
    // live `existing_peer`) only ever mutates `peer.pc` in place, never
    // touching the transport's `peers` map -- so this is really re-checking
    // the same objects captured in `peer_{a,b}_before`, not a new lookup that
    // could coincidentally match.
    let peer_a_after = ta
        .peers
        .read()
        .await
        .get(&id_b)
        .cloned()
        .expect("A should still have a live peer for B after the restart");
    let peer_b_after = tb
        .peers
        .read()
        .await
        .get(&id_a)
        .cloned()
        .expect("B should still have a live peer for A after the restart");
    assert!(
        StdArc::ptr_eq(&peer_a_before, &peer_a_after),
        "A's peer for B must be renegotiated in place, not replaced"
    );
    assert!(
        StdArc::ptr_eq(&peer_b_before, &peer_b_after),
        "B's peer for A must be renegotiated in place, not replaced"
    );

    ta.send(
        &id_b,
        bytes::Bytes::from_static(b"ping-after-ice-restart"),
        DeliveryMethod::ReliableOrdered,
    )
    .await
    .expect("data channel must still be usable after an ICE restart");
}

/// Regression test for the enhance/simulation x develop merge: after a
/// successful ICE restart the RTCPeerConnection re-enters `Connected`, but
/// the ReliableOrdered DC's `on_open` (the normal place `Connected` is set
/// since the zombie-cleanup work) never re-fires -- webrtc-rs consumes that
/// handler on first invocation. `recover_connected_from_grace` is the state
/// handler's replacement recovery path; without it the peer sits in
/// `Reconnecting` until the sweeper tears the healthy connection down at
/// grace expiry.
#[tokio::test]
async fn pc_reconnect_during_grace_recovers_connected_state() {
    let t = super::make_transport();
    let node = NodeId("grace-recovery-peer".to_string());
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connected);

    // The ICE `Disconnected` arm starts the grace and moves us to Reconnecting.
    let handles = t.peer_handles();
    let (reserved, freshly_started) = handles.mark_disconnected_grace(&node);
    assert!(reserved && freshly_started);
    assert_eq!(t.get_connection_state(&node), ConnectionState::Reconnecting);

    // A successful restart flips the pc back to `Connected`; the state
    // handler's recovery path must clear the grace and restore the state.
    assert!(handles.recover_connected_from_grace(&node));
    assert_eq!(t.get_connection_state(&node), ConnectionState::Connected);
    assert!(
        !t.disconnected_since.read().unwrap().contains_key(&node),
        "grace entry must be cleared so the sweeper cannot reap the recovered peer"
    );
}

/// The recovery path must NOT fire for a fresh connect: `Connected` there is
/// still owed to the ReliableOrdered DC actually opening (zombie rule), not
/// to the aggregate pc state flipping first.
#[tokio::test]
async fn recover_connected_from_grace_is_noop_without_pending_grace() {
    let t = super::make_transport();
    let node = NodeId("fresh-connect-peer".to_string());
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connecting);

    assert!(!t.peer_handles().recover_connected_from_grace(&node));
    assert_eq!(t.get_connection_state(&node), ConnectionState::Connecting);
}

// --- Repair-first ICE restart -------------------------------------------
//
// `ice_restart_allowed` (per-peer rate-limit decision) and `is_restart_request`
// (RestartRequest marker round trip) are pure functions, exhaustively unit
// tested below without any live `RTCPeerConnection`. The integration tests
// after them drive the real trigger sites (`mark_suspect_disconnected`,
// incoming `RestartRequest`) through a real transport/signaling pair,
// following the patterns in `tests/signaling.rs` and `tests/takeover.rs`.

mod ice_restart_allowed_tests {
    use super::ice_restart_allowed;
    use crate::transports::webrtc::ICE_RESTART_MIN_INTERVAL_MS;

    #[test]
    fn allowed_when_no_restart_ever_recorded() {
        assert!(ice_restart_allowed(None));
    }

    #[test]
    fn blocked_within_the_minimum_interval() {
        assert!(!ice_restart_allowed(Some(0)));
        assert!(!ice_restart_allowed(Some(ICE_RESTART_MIN_INTERVAL_MS - 1)));
    }

    #[test]
    fn boundary_ms_since_last_exactly_at_threshold_is_allowed() {
        // Strict-`<`-blocks convention, matching `takeover_allowed`'s own
        // boundary: `ms == threshold` no longer counts as "too recent".
        assert!(ice_restart_allowed(Some(ICE_RESTART_MIN_INTERVAL_MS)));
    }

    #[test]
    fn allowed_once_comfortably_past_the_minimum_interval() {
        assert!(ice_restart_allowed(Some(ICE_RESTART_MIN_INTERVAL_MS + 1)));
        assert!(ice_restart_allowed(Some(ICE_RESTART_MIN_INTERVAL_MS * 100)));
    }
}

mod repair_trigger_jitter_tests {
    use crate::transports::webrtc::{repair_trigger_jitter_ms, REPAIR_TRIGGER_JITTER_MS};
    use mistlib_core::types::NodeId;

    #[test]
    fn deterministic_for_a_fixed_pair() {
        let a = NodeId("peer-a".to_string());
        let b = NodeId("peer-b".to_string());
        assert_eq!(
            repair_trigger_jitter_ms(&a, &b),
            repair_trigger_jitter_ms(&a, &b),
            "the same (local_node_id, node) pair must always derive the same jitter"
        );
    }

    #[test]
    fn within_bounds_for_several_pairs() {
        let pairs = [
            (NodeId("aaa".to_string()), NodeId("bbb".to_string())),
            (NodeId("node-1".to_string()), NodeId("node-2".to_string())),
            (NodeId("zzz".to_string()), NodeId("aaa".to_string())),
            (NodeId("same".to_string()), NodeId("same".to_string())),
        ];
        for (local, node) in pairs {
            let jitter = repair_trigger_jitter_ms(&local, &node);
            assert!(
                jitter <= REPAIR_TRIGGER_JITTER_MS,
                "jitter {} for ({:?}, {:?}) exceeds REPAIR_TRIGGER_JITTER_MS ({})",
                jitter,
                local,
                node,
                REPAIR_TRIGGER_JITTER_MS
            );
        }
    }

    #[test]
    fn different_pairs_can_derive_different_jitter() {
        // Not guaranteed for every possible pair (hash collisions exist), but
        // for these two fixed pairs it demonstrates the derivation actually
        // depends on both IDs rather than e.g. always returning 0.
        let a = NodeId("peer-a".to_string());
        let b = NodeId("peer-b".to_string());
        let c = NodeId("peer-c".to_string());
        assert_ne!(
            repair_trigger_jitter_ms(&a, &b),
            repair_trigger_jitter_ms(&a, &c),
            "different `node` targets are expected to derive different jitter for this fixed pair"
        );
    }
}

mod restart_request_marker_tests {
    use super::{is_restart_request, RESTART_REQUEST_MARKER};

    #[test]
    fn round_trips_through_the_marker() {
        assert!(is_restart_request(RESTART_REQUEST_MARKER));
    }

    #[test]
    fn an_empty_data_field_is_not_a_restart_request() {
        // The ordinary CONNECT_REQUEST hint always uses an empty `data`
        // field (see `WebRtcTransport::send_connect_request`) -- it must
        // never be mistaken for a `RestartRequest`.
        assert!(!is_restart_request(""));
    }

    #[test]
    fn an_unrelated_string_is_not_a_restart_request() {
        assert!(!is_restart_request("some-other-payload"));
    }
}

/// Records every `SignalingData` sent through it (for test assertions) while
/// also forwarding the message to `tx`, exactly like `disconnect::LoopbackSignaler`
/// -- so a pair built with this can still complete a real handshake, but the
/// test can additionally inspect exactly what was sent and to whom. Needed
/// because `disconnect::LoopbackSignaler` only forwards; none of the
/// existing helpers expose the sent messages themselves.
struct RecordingLoopbackSignaler {
    tx: tokio::sync::mpsc::UnboundedSender<mistlib_core::signaling::MessageContent>,
    sent: StdArc<std::sync::Mutex<Vec<mistlib_core::signaling::SignalingData>>>,
}

#[async_trait::async_trait]
impl mistlib_core::signaling::Signaler for RecordingLoopbackSignaler {
    async fn send_signaling(
        &self,
        _to: &NodeId,
        msg: mistlib_core::signaling::MessageContent,
    ) -> mistlib_core::error::Result<()> {
        if let mistlib_core::signaling::MessageContent::Data(d) = &msg {
            self.sent.lock().unwrap().push(d.clone());
        }
        let _ = self.tx.send(msg);
        Ok(())
    }

    async fn close(&self) -> mistlib_core::error::Result<()> {
        Ok(())
    }
}

/// Sent-message log shared between a `RecordingLoopbackSignaler` and the test
/// that constructed it -- factored into a named alias (rather than inlining
/// the nested `Arc<Mutex<Vec<..>>>` at every use) per clippy's
/// `type_complexity` lint.
type SentLog = StdArc<std::sync::Mutex<Vec<mistlib_core::signaling::SignalingData>>>;

/// Same wiring as `disconnect::make_connected_pair`, but with each side's
/// signaler additionally recording every message it sends -- see
/// `RecordingLoopbackSignaler`.
fn make_connected_pair_with_recording() -> (
    StdArc<crate::transports::webrtc::WebRtcTransport>,
    StdArc<crate::transports::webrtc::WebRtcTransport>,
    NodeId,
    NodeId,
    SentLog,
    SentLog,
) {
    use crate::transports::webrtc::WebRtcTransport;
    use mistlib_core::signaling::SignalingHandler;

    let id_a = NodeId("peer-a".to_string());
    let id_b = NodeId("peer-b".to_string());

    let (tx_a_to_b, rx_a_to_b) = tokio::sync::mpsc::unbounded_channel();
    let (tx_b_to_a, rx_b_to_a) = tokio::sync::mpsc::unbounded_channel();
    let sent_by_a = StdArc::new(std::sync::Mutex::new(Vec::new()));
    let sent_by_b = StdArc::new(std::sync::Mutex::new(Vec::new()));

    let ta = StdArc::new(WebRtcTransport::new(
        StdArc::new(RecordingLoopbackSignaler {
            tx: tx_a_to_b,
            sent: sent_by_a.clone(),
        }),
        id_a.clone(),
    ));
    let tb = StdArc::new(WebRtcTransport::new(
        StdArc::new(RecordingLoopbackSignaler {
            tx: tx_b_to_a,
            sent: sent_by_b.clone(),
        }),
        id_b.clone(),
    ));

    let tb_route = tb.clone();
    tokio::spawn(async move {
        let mut rx = rx_a_to_b;
        while let Some(msg) = rx.recv().await {
            let _ = tb_route.handle_message(msg).await;
        }
    });
    let ta_route = ta.clone();
    tokio::spawn(async move {
        let mut rx = rx_b_to_a;
        while let Some(msg) = rx.recv().await {
            let _ = ta_route.handle_message(msg).await;
        }
    });

    (ta, tb, id_a, id_b, sent_by_a, sent_by_b)
}

/// Polls `sent` (a `RecordingLoopbackSignaler`'s recorded messages) until it
/// contains an entry matching `pred`, or `timeout_ms` elapses.
async fn wait_for_sent(
    sent: &std::sync::Mutex<Vec<mistlib_core::signaling::SignalingData>>,
    timeout_ms: u64,
    pred: impl Fn(&mistlib_core::signaling::SignalingData) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if sent.lock().unwrap().iter().any(&pred) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Change 2, REVISED for the degraded-transport gate: `mark_suspect_disconnected`
/// on the initiator side (A, the lower node ID toward B) used to fire an
/// actual ICE-restart repair attempt -- a fresh `Offer` with `ice_restart:
/// true` on the existing peer connection -- unconditionally once a grace
/// started. `PeerSharedHandles::maybe_try_ice_restart` now gates that on the
/// target pc actually being degraded (not `Connected`) first (see its doc
/// comment for the fault-injection evidence: restarting a healthy pc wipes
/// the answerer's candidate pairs and kills the session). `suspect_disconnected`
/// is only an app-level liveness hunch (missed PONGs on the best-effort
/// channel), so the pc here never actually left `Connected` -- the gate must
/// skip the restart entirely and no fresh Offer is sent.
///
/// multi_thread required: see the reasoning in `disconnect.rs`/`signaling.rs`
/// -- A/B are independent peers and must not share an OS thread for
/// realistic scheduling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn liveness_suspect_on_initiator_side_with_healthy_pc_sends_no_restart_offer() {
    use crate::transports::webrtc::{REPAIR_TRIGGER_DEBOUNCE_MS, REPAIR_TRIGGER_JITTER_MS};
    use mistlib_core::signaling::SignalingType;

    let (ta, tb, id_a, id_b, sent_by_a, _sent_by_b) = make_connected_pair_with_recording();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );
    assert!(is_ice_restart_initiator(&id_a, &id_b));

    // Establishment itself already sent an Offer -- clear it so the
    // assertion below is only defeated by a *new* one.
    sent_by_a.lock().unwrap().clear();

    ta.suspect_disconnected(&id_b)
        .await
        .expect("suspect_disconnected should not fail");

    // Wait comfortably past the full debounce+jitter window (test-mode
    // budget: REPAIR_TRIGGER_DEBOUNCE_MS + up to REPAIR_TRIGGER_JITTER_MS,
    // 20+10ms) -- long enough that the pre-gate behavior would certainly have
    // already sent an offer -- before asserting nothing was sent.
    tokio::time::sleep(std::time::Duration::from_millis(
        REPAIR_TRIGGER_DEBOUNCE_MS + REPAIR_TRIGGER_JITTER_MS + 500,
    ))
    .await;

    assert!(
        !sent_by_a
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.signaling_type == SignalingType::Offer && d.receiver_id == id_b),
        "the degraded-transport gate must skip the restart offer while the pc is still Connected"
    );
}

/// Change 3: `mark_suspect_disconnected` on the NON-initiator side (B, the
/// higher node ID toward A) has no PC-level ICE-restart trigger of its own,
/// so it must send a `RestartRequest` nudge instead of doing nothing (the
/// pre-fix behavior: the higher-ID side could only wait).
///
/// multi_thread required: see the reasoning in `disconnect.rs`/`signaling.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn liveness_suspect_on_non_initiator_side_sends_a_restart_request() {
    use mistlib_core::signaling::SignalingType;

    let (ta, tb, id_a, id_b, _sent_by_a, sent_by_b) = make_connected_pair_with_recording();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );
    assert!(!is_ice_restart_initiator(&id_b, &id_a));

    sent_by_b.lock().unwrap().clear();

    tb.suspect_disconnected(&id_a)
        .await
        .expect("suspect_disconnected should not fail");

    assert!(
        wait_for_sent(&sent_by_b, 5_000, |d| d.signaling_type
            == SignalingType::Request
            && d.receiver_id == id_a
            && is_restart_request(&d.data))
        .await,
        "the non-initiator must send a RestartRequest nudge in response to liveness suspicion"
    );
}

/// Change 3 (receive side), REVISED for the degraded-transport gate: an
/// incoming `RestartRequest` for a peer we still have a live session for used
/// to trigger a rate-limited ICE restart (a fresh `Offer`) unconditionally.
/// `maybe_try_ice_restart`'s gate now refuses to act while the target pc is
/// still `Connected` -- and here it genuinely is, since nothing in this test
/// actually degraded the transport, only the *requester*'s side believed it
/// might be broken. No Offer must be sent.
///
/// Uses a real connected pair (rather than a bare `create_pc` without ever
/// negotiating) so the pc reports a real `Connected` state for the gate to
/// observe, matching every production target of this handler.
///
/// `handle_message` itself returns as soon as the debounced repair task is
/// spawned (`handle_restart_request` no longer blocks on the debounce window
/// or the restart attempt), so this waits out the full debounce+jitter delay
/// (test-mode `REPAIR_TRIGGER_DEBOUNCE_MS` + up to `REPAIR_TRIGGER_JITTER_MS`,
/// 20+10ms) with margin before asserting nothing was sent.
///
/// multi_thread required: see the reasoning in `disconnect.rs`/`signaling.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_request_with_existing_healthy_session_sends_no_restart_offer() {
    use crate::transports::webrtc::{REPAIR_TRIGGER_DEBOUNCE_MS, REPAIR_TRIGGER_JITTER_MS};
    use mistlib_core::signaling::{MessageContent, SignalingData, SignalingHandler, SignalingType};

    let (ta, tb, id_a, id_b, sent_by_a, _sent_by_b) = make_connected_pair_with_recording();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    sent_by_a.lock().unwrap().clear();

    // Simulate B sending A a RestartRequest directly (bypassing
    // `send_restart_request` itself, which is exercised separately by
    // `liveness_suspect_on_non_initiator_side_sends_a_restart_request`) --
    // this isolates the receive-side handler.
    let msg = MessageContent::Data(SignalingData {
        sender_id: id_b.clone(),
        receiver_id: id_a.clone(),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: RESTART_REQUEST_MARKER.to_string(),
    });
    assert!(ta.handle_message(msg).await.is_ok());

    tokio::time::sleep(std::time::Duration::from_millis(
        REPAIR_TRIGGER_DEBOUNCE_MS + REPAIR_TRIGGER_JITTER_MS + 500,
    ))
    .await;

    assert!(
        !sent_by_a
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.signaling_type == SignalingType::Offer && d.receiver_id == id_b),
        "an incoming RestartRequest for a healthy (still-Connected) session must not produce a \
         restart offer -- the degraded-transport gate skips it"
    );
}

/// Change 3 (receive side): an incoming `RestartRequest` for a peer we have
/// NO session for must be ignored entirely -- it must never itself create a
/// connection (reconnection initiation stays with the overlay
/// balancer/CONNECT_REQUEST flow, unlike the ordinary `SignalingType::Request`
/// handling a few lines below it in `signaling.rs`, which can call `connect`).
#[tokio::test]
async fn restart_request_without_existing_session_is_ignored() {
    use mistlib_core::signaling::{MessageContent, SignalingData, SignalingHandler, SignalingType};

    let t = super::make_transport();
    let remote = NodeId("remote-with-no-session".to_string());

    let msg = MessageContent::Data(SignalingData {
        sender_id: remote.clone(),
        receiver_id: NodeId("local".to_string()),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: RESTART_REQUEST_MARKER.to_string(),
    });
    assert!(t.handle_message(msg).await.is_ok());

    assert!(
        !t.peers.read().await.contains_key(&remote),
        "a RestartRequest for an unknown peer must not create a new session"
    );
    assert_eq!(
        t.get_connection_state(&remote),
        ConnectionState::Disconnected,
        "a RestartRequest for an unknown peer must not reserve connection state either"
    );
}

/// Change 4, REVISED for the degraded-transport gate: a second restart
/// trigger for the same peer within `ICE_RESTART_MIN_INTERVAL_MS` must be
/// skipped by the rate limit -- only the first actually gets admitted.
///
/// A still-`Connected` pc would now be skipped by `maybe_try_ice_restart`'s
/// gate before the rate limit is even consulted (see its doc comment), which
/// would make this test pass for the wrong reason (or not at all, since a
/// pc that never gets admitted never records `last_ice_restart_at`). So this
/// closes A's view of the pc first, exactly like `admitted_restart_rearms_the_disconnect_grace`,
/// to move it out of `Connected` and let both calls reach the rate limiter.
/// A side effect of a closed pc is that `try_ice_restart_once`'s
/// `create_offer` call itself fails (no ICE agent left to restart), so an
/// actually-sent Offer is no longer a usable "was this call admitted" signal
/// here -- `last_ice_restart_at` (the rate limiter's own bookkeeping,
/// written only when a call is actually admitted) is used instead: the first
/// call must record a fresh timestamp, and the second call, arriving well
/// within `ICE_RESTART_MIN_INTERVAL_MS`, must leave that timestamp untouched.
///
/// multi_thread required: see the reasoning in `disconnect.rs`/`signaling.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_restart_within_the_minimum_interval_is_skipped() {
    let (ta, tb, id_a, id_b, sent_by_a, _sent_by_b) = make_connected_pair_with_recording();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    sent_by_a.lock().unwrap().clear();

    let peer_a = ta
        .peers
        .read()
        .await
        .get(&id_b)
        .cloned()
        .expect("A should have a live peer for B");
    // Cancel this Peer's own token first so its `Closed` state-change
    // callback (`setup_connection_state_handler`) treats itself as already
    // torn down and no-ops instead of racing `remove_peer_if_current` out
    // from under this test -- see that callback's `cancel_token.is_cancelled()`
    // early return. Without this, `pc.close()` below asynchronously removes
    // `id_b` from `ta.peers`/`disconnected_since` well before this test gets
    // to observe the gate, since that cleanup is itself `tokio::spawn`ed off
    // the state-change event.
    peer_a.cancel_token.cancel();
    peer_a
        .pc
        .close()
        .await
        .expect("closing the pc should not fail");

    let handles = ta.peer_handles();
    // Two triggers back to back, well within `ICE_RESTART_MIN_INTERVAL_MS`
    // (200ms under `#[cfg(test)]`) of each other -- the second must be
    // skipped by the rate limit, not attempted.
    handles.maybe_try_ice_restart(&id_b, "test").await;
    let after_first = handles
        .last_ice_restart_at
        .read()
        .unwrap()
        .get(&id_b)
        .copied();
    assert!(
        after_first.is_some(),
        "the first (admitted) restart trigger must record last_ice_restart_at"
    );

    handles.maybe_try_ice_restart(&id_b, "test").await;
    let after_second = handles
        .last_ice_restart_at
        .read()
        .unwrap()
        .get(&id_b)
        .copied();
    assert_eq!(
        after_first, after_second,
        "the second trigger within the minimum interval must be rate-limited -- it must not \
         update last_ice_restart_at again"
    );
}

/// Storm-avoidance fix: if a disconnect grace clears during
/// `spawn_repair_trigger`'s debounce window (self-recovery -- see
/// `REPAIR_TRIGGER_DEBOUNCE_MS`'s doc comment for the measured repair-storm
/// regression this closes), the repair action must be skipped entirely on
/// BOTH sides -- no restart offer from the initiator, no `RestartRequest`
/// from the non-initiator.
///
/// Recovers each side's grace with `recover_connected_from_grace` right after
/// `suspect_disconnected` returns, well inside the debounce window (20ms +
/// up to 10ms jitter under `#[cfg(test)]`) since nothing between grace-start
/// and that call awaits anything -- then waits comfortably past the full
/// debounce+jitter window before asserting nothing was sent.
///
/// multi_thread required: see the reasoning in `disconnect.rs`/`signaling.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grace_cleared_during_debounce_window_skips_the_repair_action() {
    use crate::transports::webrtc::{REPAIR_TRIGGER_DEBOUNCE_MS, REPAIR_TRIGGER_JITTER_MS};
    use mistlib_core::signaling::SignalingType;

    let (ta, tb, id_a, id_b, sent_by_a, sent_by_b) = make_connected_pair_with_recording();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );
    assert!(is_ice_restart_initiator(&id_a, &id_b));

    sent_by_a.lock().unwrap().clear();
    sent_by_b.lock().unwrap().clear();

    // Suspect the link on both sides so both the initiator's
    // (`maybe_try_ice_restart`) and non-initiator's (`send_restart_request`)
    // branches of `spawn_repair_trigger` are exercised at once.
    ta.suspect_disconnected(&id_b)
        .await
        .expect("suspect_disconnected should not fail");
    tb.suspect_disconnected(&id_a)
        .await
        .expect("suspect_disconnected should not fail");

    // Recover both graces immediately -- simulating the wake-race
    // self-recovering before the debounced repair action gets to run.
    assert!(ta.peer_handles().recover_connected_from_grace(&id_b));
    assert!(tb.peer_handles().recover_connected_from_grace(&id_a));

    // Wait past the full debounce+jitter window with margin -- long enough
    // that the repair action would certainly have fired by now if the
    // debounce-skip re-check didn't work.
    tokio::time::sleep(std::time::Duration::from_millis(
        REPAIR_TRIGGER_DEBOUNCE_MS + REPAIR_TRIGGER_JITTER_MS + 500,
    ))
    .await;

    assert!(
        !sent_by_a
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.signaling_type == SignalingType::Offer && d.receiver_id == id_b),
        "no restart offer should be sent once the grace recovered during the debounce window"
    );
    assert!(
        !sent_by_b
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.signaling_type == SignalingType::Request
                && d.receiver_id == id_a
                && is_restart_request(&d.data)),
        "no RestartRequest should be sent once the grace recovered during the debounce window"
    );
}

// --- ICE restart as rescue, not reflex ----------------------------------
//
// The two tests below cover the remaining two changes from that redesign:
// the grace re-arm (`PeerSharedHandles::rearm_disconnect_grace`, exercised
// through `maybe_try_ice_restart`) and the receive-side `RestartRequest`
// debounce (`WebRtcTransport::handle_restart_request`).

/// Change 2 ("rescue window"): an admitted ICE-restart attempt (one that
/// passes `ice_restart_allowed`'s per-peer rate limit) must refresh its
/// peer's disconnect grace `started_at` to `Instant::now()`, giving the
/// just-admitted repair attempt a fresh `DISCONNECTED_GRACE_MS` window to
/// complete instead of racing a clock that had already been running since
/// the grace began -- see `PeerSharedHandles::rearm_disconnect_grace`'s doc
/// comment for the measured grace-expiry-teardown regression this closes.
///
/// Uses a real connected pair (same reason as the other
/// `maybe_try_ice_restart` tests above -- `create_offer(ice_restart: true)`
/// requires an existing ICE agent) but drives the timing entirely through a
/// synthetic, already-old `disconnected_since` entry rather than actually
/// waiting out `DISCONNECTED_GRACE_MS`, which would make this test both slow
/// and racy against the sweeper.
///
/// Degraded-transport gate: `maybe_try_ice_restart` now refuses to act at all
/// on a still-`Connected` pc (see its doc comment), so this closes A's view
/// of the pc first -- moving its `connection_state()` to `Closed` while
/// leaving its `peers`/`disconnected_since` bookkeeping in place -- to let
/// the call reach the rearm this test is actually about. The restart attempt
/// itself is expected to no-op after that (`create_offer` on a closed pc
/// fails, see `try_ice_restart_once`) -- that's fine, this test's subject is
/// the rearm, which fires on *admission* (passing the gate + rate limit),
/// not on the attempt's success.
///
/// multi_thread required: see the reasoning in `disconnect.rs`/`signaling.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admitted_restart_rearms_the_disconnect_grace() {
    use crate::transports::webrtc::{DisconnectGrace, GraceOrigin};
    use std::time::{Duration, Instant};

    let (ta, tb, id_a, id_b, sent_by_a, _sent_by_b) = make_connected_pair_with_recording();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    sent_by_a.lock().unwrap().clear();

    // Degrade A's view of the pc so the gate lets this call through at all.
    // The peer's own token is cancelled first so its `Closed` state-change
    // callback no-ops (see `cancel_token.is_cancelled()` in
    // `setup_connection_state_handler`) instead of racing this test's own
    // `disconnected_since`/`peers` bookkeeping via its `tokio::spawn`ed
    // cleanup.
    let peer_a = ta
        .peers
        .read()
        .await
        .get(&id_b)
        .cloned()
        .expect("A should have a live peer for B");
    peer_a.cancel_token.cancel();
    peer_a
        .pc
        .close()
        .await
        .expect("closing the pc should not fail");

    // Simulate a grace that started well in the past -- the "restart admitted
    // late in the grace's lifetime" scenario the rescue window exists for.
    let old_started_at = Instant::now() - Duration::from_millis(1_000);
    ta.disconnected_since.write().unwrap().insert(
        id_b.clone(),
        DisconnectGrace {
            started_at: old_started_at,
            origin: GraceOrigin::Ice,
        },
    );

    ta.peer_handles().maybe_try_ice_restart(&id_b, "test").await;

    let refreshed_started_at = ta
        .disconnected_since
        .read()
        .unwrap()
        .get(&id_b)
        .expect("grace entry must still be present after an admitted restart")
        .started_at;
    assert!(
        refreshed_started_at > old_started_at,
        "an admitted restart must reset started_at to a fresh Instant::now(), not leave the \
         original grace-start timestamp in place"
    );
    assert!(
        refreshed_started_at.elapsed() < Duration::from_millis(500),
        "the refreshed started_at must be recent (close to when the restart was admitted), got \
         {:?} old",
        refreshed_started_at.elapsed()
    );
}

/// Direct unit-level coverage of `maybe_try_ice_restart`'s degraded-transport
/// gate itself, isolated from the higher-level trigger paths
/// (`suspect_disconnected`, incoming `RestartRequest`) covered elsewhere in
/// this file -- both of those now skip for the same underlying reason on a
/// genuinely healthy pair, but neither pins down the gate's boundary as
/// directly as calling `maybe_try_ice_restart` itself, before and after the
/// same peer's pc degrades.
///
/// First call: the pc is still `Connected` (real connected pair, nothing
/// disrupted it) -- the gate must skip before ever reaching the rate limiter
/// or the rearm, so a synthetic, seeded grace's `started_at` must be left
/// completely untouched and no offer sent. Second call, after closing that
/// same pc (`connection_state()` moves off `Connected`): the gate now admits
/// the attempt, so the grace's `started_at` must be refreshed -- the same
/// rearm `admitted_restart_rearms_the_disconnect_grace` covers, here shown as
/// the direct contrast to the first call's skip.
///
/// multi_thread required: see the reasoning in `disconnect.rs`/`signaling.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maybe_try_ice_restart_gate_skips_connected_pc_and_admits_once_degraded() {
    use crate::transports::webrtc::{DisconnectGrace, GraceOrigin};
    use mistlib_core::signaling::SignalingType;
    use std::time::{Duration, Instant};

    let (ta, tb, id_a, id_b, sent_by_a, _sent_by_b) = make_connected_pair_with_recording();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    sent_by_a.lock().unwrap().clear();

    // Seed a grace so a would-be rearm is observable either way.
    let old_started_at = Instant::now() - Duration::from_millis(1_000);
    ta.disconnected_since.write().unwrap().insert(
        id_b.clone(),
        DisconnectGrace {
            started_at: old_started_at,
            origin: GraceOrigin::Ice,
        },
    );

    // --- Still Connected: the gate must skip entirely. ---
    ta.peer_handles().maybe_try_ice_restart(&id_b, "test").await;

    assert_eq!(
        ta.disconnected_since
            .read()
            .unwrap()
            .get(&id_b)
            .expect("grace entry must still be present")
            .started_at,
        old_started_at,
        "a still-Connected pc must not have its grace rearmed -- the gate must skip before ever \
         reaching the rearm"
    );
    assert!(
        !sent_by_a
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.signaling_type == SignalingType::Offer && d.receiver_id == id_b),
        "a still-Connected pc must not produce a restart offer"
    );

    // --- Degrade the pc: the gate must now admit the attempt. ---
    // Cancel the peer's own token first so its `Closed` state-change
    // callback no-ops instead of racing this test's own `peers`/
    // `disconnected_since` bookkeeping via its `tokio::spawn`ed cleanup (see
    // `cancel_token.is_cancelled()` in `setup_connection_state_handler`).
    let peer_a = ta
        .peers
        .read()
        .await
        .get(&id_b)
        .cloned()
        .expect("A should still have a live peer for B");
    peer_a.cancel_token.cancel();
    peer_a
        .pc
        .close()
        .await
        .expect("closing the pc should not fail");

    ta.peer_handles().maybe_try_ice_restart(&id_b, "test").await;

    let refreshed_started_at = ta
        .disconnected_since
        .read()
        .unwrap()
        .get(&id_b)
        .expect("grace entry must still be present after an admitted restart")
        .started_at;
    assert!(
        refreshed_started_at > old_started_at,
        "once the pc is no longer Connected, an admitted restart must rearm the grace"
    );
}

/// Change 3 (receive-side debounce): a `RestartRequest` for a peer whose
/// session is torn down WHILE the receive-side debounce is still sleeping
/// must not trigger a restart offer -- the post-delay re-check in
/// `handle_restart_request` must see the peer gone and bail out, the same
/// outcome as `restart_request_without_existing_session_is_ignored` but
/// reached via the debounce path (the pre-fix code checked `has_peer`
/// immediately, before any delay existed).
///
/// multi_thread required: see the reasoning in `disconnect.rs`/`signaling.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_request_torn_down_during_the_debounce_window_sends_nothing() {
    use crate::transports::webrtc::{REPAIR_TRIGGER_DEBOUNCE_MS, REPAIR_TRIGGER_JITTER_MS};
    use mistlib_core::signaling::{MessageContent, SignalingData, SignalingHandler, SignalingType};

    let (ta, tb, id_a, id_b, sent_by_a, _sent_by_b) = make_connected_pair_with_recording();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    sent_by_a.lock().unwrap().clear();

    let msg = MessageContent::Data(SignalingData {
        sender_id: id_b.clone(),
        receiver_id: id_a.clone(),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: RESTART_REQUEST_MARKER.to_string(),
    });
    // `handle_message` now only spawns the debounced repair task and returns
    // immediately -- it no longer waits for (or even starts) the actual
    // restart attempt.
    assert!(ta.handle_message(msg).await.is_ok());

    // Tear down A's session for B right away, well before the receive-side
    // debounce (test-mode budget: REPAIR_TRIGGER_DEBOUNCE_MS + up to
    // REPAIR_TRIGGER_JITTER_MS, 20+10ms) has elapsed.
    ta.cleanup_session(&id_b, true).await;
    assert!(
        !ta.peers.read().await.contains_key(&id_b),
        "the session must actually be gone before the debounce window ends"
    );

    // Wait comfortably past the full debounce+jitter window -- long enough
    // that the pre-fix immediate-check behavior would certainly have already
    // sent an offer -- before asserting nothing was sent.
    tokio::time::sleep(std::time::Duration::from_millis(
        REPAIR_TRIGGER_DEBOUNCE_MS + REPAIR_TRIGGER_JITTER_MS + 500,
    ))
    .await;

    assert!(
        !sent_by_a
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.signaling_type == SignalingType::Offer && d.receiver_id == id_b),
        "no restart offer should be sent for a peer torn down during the receive-side debounce \
         window"
    );
}
