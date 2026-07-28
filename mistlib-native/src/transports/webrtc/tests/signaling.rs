use super::disconnect::{make_connected_pair, wait_for_state};
use super::*;
use mistlib_core::signaling::{MessageContent, SignalingData, SignalingHandler, SignalingType};
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, NodeId};

// --- Rejoin ------------------------------------------------------------
//
// A node's Nostr signaling keypair regenerates on every restart while its
// `NodeId` stays stable across restarts. The signaling layer
// (`NostrSignaler::process_event`) detects this and injects a locally-only
// `SignalingType::Rejoin` for the restarted peer into the incoming stream
// *before* forwarding the peer's real Offer/Request that arrives immediately
// after -- see `SignalingType::Rejoin`'s doc comment. The transport's job,
// exercised here, is to tear down the stale peer connection on that
// notification: an abruptly-vanished WebRTC peer still reports
// `readyState == Open` locally for tens of seconds, so without this the
// peer's real reconnection attempt would be misapplied to (or lost racing
// against) a corpse `RTCPeerConnection`.

/// Scenario 5: a `Rejoin` for a node with a live peer session tears that
/// session down, via the same guarded `cleanup_session_if_current` machinery
/// used by every other "supersede a stale peer" path in this module (the
/// remote-takeover paths, the connect watchdog, the periodic sweeper).
///
/// multi_thread required: see the reasoning on the disconnect-detection tests
/// in `disconnect.rs` -- A/B are independent peers and must not share an OS
/// thread for realistic scheduling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejoin_tears_down_a_live_peer_session() {
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
    assert!(
        tb.peers.read().await.contains_key(&id_a),
        "sanity: B should have a live peer for A before the Rejoin arrives"
    );

    let rejoin = MessageContent::Data(SignalingData {
        sender_id: id_a.clone(),
        receiver_id: id_b.clone(),
        room_id: String::new(),
        data: "123456".to_string(),
        signaling_type: SignalingType::Rejoin,
    });
    tb.handle_message(rejoin)
        .await
        .expect("Rejoin handling must not fail");

    assert!(
        !tb.peers.read().await.contains_key(&id_a),
        "the stale peer session for A must be torn down once B learns A rebound"
    );
    assert_eq!(
        tb.get_connection_state(&id_a),
        ConnectionState::Disconnected,
        "B's connection state for A must reflect the teardown, not linger as Connected"
    );
}

/// A `Rejoin` for a node with no live peer session at all must be a
/// complete no-op -- nothing to tear down, and no panic/error either.
#[tokio::test]
async fn rejoin_with_no_live_peer_is_a_noop() {
    let t = make_transport();
    let node = NodeId("never-connected".to_string());

    let rejoin = MessageContent::Data(SignalingData {
        sender_id: node.clone(),
        receiver_id: NodeId("local".to_string()),
        room_id: String::new(),
        data: "0".to_string(),
        signaling_type: SignalingType::Rejoin,
    });

    assert!(t.handle_message(rejoin).await.is_ok());
    assert_eq!(t.get_connection_state(&node), ConnectionState::Disconnected);
}

/// Reproduces the reconnect-flap wedge: a signaling send that fails *after*
/// `send_offer` already committed a local offer (`set_local_description`)
/// used to leave the peer's `RTCPeerConnection` stuck at `HaveLocalOffer`
/// forever, since nothing rolled the signaling state back to `Stable`. In
/// production this is exactly what a `RouteNotFound` from `RoutedSignaler`
/// does when a peer's overlay route hasn't caught up with a just-established
/// connection yet (routing table sync runs on a ~1s tick) -- the follow-up
/// offer/renegotiation is dropped, and the peer is left rejecting every
/// later offer/renegotiation attempt (the "signaling state is not
/// stable"/glare precondition) until the whole connection is torn down and
/// rebuilt from scratch, which is the observed flap.
///
/// multi_thread required: see the reasoning on the disconnect-detection
/// tests in `disconnect.rs` -- A/B are independent peers and must not share
/// an OS thread for realistic scheduling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_offer_send_rolls_back_instead_of_wedging_the_peer() {
    use async_trait::async_trait;
    use mistlib_core::error::{MistError, Result as MistResult};
    use mistlib_core::signaling::Signaler;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use webrtc::peer_connection::signaling_state::RTCSignalingState;

    /// Like `disconnect::LoopbackSignaler`, but can be told to fail exactly
    /// the next `send_signaling` call -- simulating `RoutedSignaler`
    /// returning `RouteNotFound` for one specific send.
    struct FlakySignaler {
        tx: mpsc::UnboundedSender<MessageContent>,
        fail_next: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Signaler for FlakySignaler {
        async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> MistResult<()> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(MistError::RouteNotFound(to.clone()));
            }
            let _ = self.tx.send(msg);
            Ok(())
        }

        async fn close(&self) -> MistResult<()> {
            Ok(())
        }
    }

    let id_a = NodeId("peer-a".to_string());
    let id_b = NodeId("peer-b".to_string());
    let (tx_a_to_b, rx_a_to_b) = mpsc::unbounded_channel::<MessageContent>();
    let (tx_b_to_a, rx_b_to_a) = mpsc::unbounded_channel::<MessageContent>();

    let a_fail_next = Arc::new(AtomicBool::new(false));
    let ta = Arc::new(WebRtcTransport::new(
        Arc::new(FlakySignaler {
            tx: tx_a_to_b,
            fail_next: a_fail_next.clone(),
        }),
        id_a.clone(),
    ));
    let tb = Arc::new(WebRtcTransport::new(
        Arc::new(FlakySignaler {
            tx: tx_b_to_a,
            fail_next: Arc::new(AtomicBool::new(false)),
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

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    // Simulate the exact production failure: the RoutedSignaler drops A's
    // next signaling send (e.g. RouteNotFound because the overlay route to
    // B hasn't caught up with the connection that was *just* established).
    a_fail_next.store(true, Ordering::SeqCst);

    let peer_a_to_b = {
        let peers = ta.peers.read().await;
        peers
            .get(&id_b)
            .cloned()
            .expect("A should have a live peer for B")
    };
    ta.send_offer(&id_b, &peer_a_to_b)
        .await
        .expect_err("the injected signaling failure must surface to the caller");

    // webrtc-rs 0.13 rejects every rollback transition
    // (`check_next_signaling_state` has no `Rollback` arm out of any
    // non-`Stable` state), so unlike a browser, the state CANNOT be brought
    // back to `Stable` here -- `rollback_to_stable_on_failure` is best-effort
    // and expected to fail. The recovery contract is
    // `Peer::local_offer_unsent` instead: the lost offer never reached the
    // remote, so `send_offer` is allowed to re-offer straight from
    // `HaveLocalOffer` (a valid `SetLocal(offer)` transition).
    assert_eq!(
        peer_a_to_b.pc.signaling_state(),
        RTCSignalingState::HaveLocalOffer,
        "the lost offer stays applied locally (webrtc-rs cannot roll back)"
    );
    assert!(
        peer_a_to_b
            .local_offer_unsent
            .load(std::sync::atomic::Ordering::SeqCst),
        "a failed offer send must mark the pending local offer as undelivered"
    );

    // Prove the peer actually recovered: a fresh renegotiation attempt must
    // now succeed. Pre-fix, this fails `can_send_offer`'s "signaling is not
    // stable" precondition because signaling_state is still HaveLocalOffer
    // and nothing recorded that the pending offer was never delivered.
    ta.send_offer(&id_b, &peer_a_to_b)
        .await
        .expect("peer must be renegotiable again after a lost offer send");
    assert!(
        !peer_a_to_b
            .local_offer_unsent
            .load(std::sync::atomic::Ordering::SeqCst),
        "a successful re-offer must clear the undelivered-offer flag"
    );
}

#[tokio::test]
async fn handle_message_request_does_not_crash() {
    let t = make_transport();
    let msg = MessageContent::Data(SignalingData {
        sender_id: NodeId("remote".to_string()),
        receiver_id: NodeId("local".to_string()),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: String::new(),
    });
    assert!(t.handle_message(msg).await.is_ok());
}

#[tokio::test]
async fn direct_request_to_local_is_ignored_when_sender_sorts_before_local() {
    let t = make_transport();
    let remote = NodeId("aaa".to_string());
    let msg = MessageContent::Data(SignalingData {
        sender_id: remote.clone(),
        receiver_id: NodeId("local".to_string()),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: String::new(),
    });

    assert!(t.handle_message(msg).await.is_ok());
    assert_eq!(
        t.get_connection_state(&remote),
        ConnectionState::Disconnected
    );
}

#[tokio::test]
async fn direct_request_to_lower_id_local_starts_connect() {
    let t = make_transport();
    let remote = NodeId("zzz".to_string());
    let msg = MessageContent::Data(SignalingData {
        sender_id: remote.clone(),
        receiver_id: NodeId("local".to_string()),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: String::new(),
    });

    assert!(t.handle_message(msg).await.is_ok());
    assert_eq!(t.get_connection_state(&remote), ConnectionState::Connecting);
}

#[tokio::test]
async fn handle_message_request_at_max_does_not_crash() {
    use mistlib_core::signaling::SignalingHandler;
    let t = make_transport();
    t.set_max_connections(0);
    let msg = MessageContent::Data(SignalingData {
        sender_id: NodeId("zzz".to_string()),
        receiver_id: NodeId("local".to_string()),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: String::new(),
    });
    let result = t.handle_message(msg).await;
    assert!(result.is_ok());
    assert_eq!(
        t.get_active_connection_states().len(),
        0,
        "No connection must be added when max=0"
    );
}

#[tokio::test]
async fn handle_message_unknown_type_does_not_crash() {
    use mistlib_core::signaling::SignalingHandler;
    let t = make_transport();
    let msg = MessageContent::Raw(b"garbage".to_vec().into());
    assert!(t.handle_message(msg).await.is_ok());
}

#[tokio::test]
async fn handle_message_request_different_room_is_ignored() {
    use mistlib_core::signaling::SignalingHandler;
    let t = make_transport();
    t.set_room_id("roomA".to_string());

    let msg = MessageContent::Data(SignalingData {
        sender_id: NodeId("remote".to_string()),
        receiver_id: NodeId("local".to_string()),
        room_id: "roomB".to_string(),
        signaling_type: SignalingType::Request,
        data: String::new(),
    });

    assert!(t.handle_message(msg).await.is_ok());
    assert_eq!(t.get_active_connection_states().len(), 0);
}

#[tokio::test]
async fn many_concurrent_requests_do_not_crash() {
    use mistlib_core::signaling::SignalingHandler;
    use std::sync::Arc as StdArc;
    const MAX: u32 = 3;
    let t = StdArc::new(make_transport());
    t.set_max_connections(MAX);

    let mut handles = Vec::new();
    for i in 0..20u32 {
        let tc = StdArc::clone(&t);
        handles.push(tokio::spawn(async move {
            let msg = MessageContent::Data(SignalingData {
                sender_id: NodeId(format!("zzz-{i}")),
                receiver_id: NodeId("local".to_string()),
                room_id: String::new(),
                signaling_type: SignalingType::Request,
                data: String::new(),
            });
            let _ = tc.handle_message(msg).await;
        }));
    }
    for h in handles {
        assert!(h.await.is_ok(), "task must not panic");
    }

    let count = t.get_active_connection_states().len();
    assert!(
        count <= MAX as usize,
        "active ({count}) must not exceed max ({MAX})"
    );
}

#[tokio::test]
async fn higher_id_connect_sends_request_without_local_offer_state() {
    use async_trait::async_trait;
    use mistlib_core::error::Result as MistResult;
    use mistlib_core::signaling::Signaler;
    use std::sync::{Arc, Mutex};

    struct RecordingSignaler(Mutex<Vec<(NodeId, MessageContent)>>);

    #[async_trait]
    impl Signaler for RecordingSignaler {
        async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> MistResult<()> {
            self.0.lock().unwrap().push((to.clone(), msg));
            Ok(())
        }

        async fn close(&self) -> MistResult<()> {
            Ok(())
        }
    }

    let signaler = Arc::new(RecordingSignaler(Mutex::new(Vec::new())));
    let t = WebRtcTransport::new(signaler.clone(), NodeId("zzz".to_string()));
    let lower = NodeId("aaa".to_string());

    assert!(t.connect(&lower).await.is_ok());
    assert_eq!(
        t.get_connection_state(&lower),
        ConnectionState::Disconnected
    );

    let sent = signaler.0.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, lower);
    match &sent[0].1 {
        MessageContent::Data(data) => {
            assert_eq!(data.signaling_type, SignalingType::Request);
            assert_eq!(data.receiver_id, NodeId("aaa".to_string()));
        }
        other => panic!("unexpected signaling message: {other:?}"),
    }
}

#[tokio::test]
async fn higher_id_connect_deduplicates_pending_request_retries() {
    use async_trait::async_trait;
    use mistlib_core::error::Result as MistResult;
    use mistlib_core::signaling::Signaler;
    use std::sync::{Arc, Mutex};

    struct RecordingSignaler(Mutex<Vec<(NodeId, MessageContent)>>);

    #[async_trait]
    impl Signaler for RecordingSignaler {
        async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> MistResult<()> {
            self.0.lock().unwrap().push((to.clone(), msg));
            Ok(())
        }

        async fn close(&self) -> MistResult<()> {
            Ok(())
        }
    }

    let signaler = Arc::new(RecordingSignaler(Mutex::new(Vec::new())));
    let t = WebRtcTransport::new(signaler.clone(), NodeId("zzz".to_string()));
    let lower = NodeId("aaa".to_string());

    assert!(t.connect(&lower).await.is_ok());
    assert!(t.connect(&lower).await.is_ok());

    assert_eq!(signaler.0.lock().unwrap().len(), 1);
    assert_eq!(t.connect_request_attempt_ids.read().unwrap().len(), 1);
}

/// Regression test for the CONNECT_REQUEST "permanent silence" investigation:
/// once a peer's `connect_request_attempt_ids` bookkeeping is cleared --
/// which is exactly what happens both when `spawn_connect_request_retry`'s
/// backoff schedule exhausts and (after the `retry_limit <= 1` leak fix) when
/// there are no retries configured at all -- a later `connect()` call for the
/// same still-unconnected node must be able to start a brand-new request
/// cycle (fresh attempt id, an actual signaling send), not be silently
/// swallowed forever by `reserve_connect_request_attempt`'s "already
/// pending" guard. Exercised by clearing the bookkeeping directly rather
/// than waiting out a real retry schedule, so this stays fast and
/// deterministic instead of depending on `spawn_connect_request_retry`'s
/// background timing.
#[tokio::test]
async fn higher_id_connect_request_restarts_after_bookkeeping_is_cleared() {
    use async_trait::async_trait;
    use mistlib_core::error::Result as MistResult;
    use mistlib_core::signaling::Signaler;
    use std::sync::{Arc, Mutex};

    struct RecordingSignaler(Mutex<Vec<(NodeId, MessageContent)>>);

    #[async_trait]
    impl Signaler for RecordingSignaler {
        async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> MistResult<()> {
            self.0.lock().unwrap().push((to.clone(), msg));
            Ok(())
        }

        async fn close(&self) -> MistResult<()> {
            Ok(())
        }
    }

    let signaler = Arc::new(RecordingSignaler(Mutex::new(Vec::new())));
    let t = WebRtcTransport::new(signaler.clone(), NodeId("zzz".to_string()));
    let lower = NodeId("aaa".to_string());

    assert!(t.connect(&lower).await.is_ok());
    assert_eq!(
        signaler.0.lock().unwrap().len(),
        1,
        "first Request must be sent"
    );
    let first_attempt_id = *t
        .connect_request_attempt_ids
        .read()
        .unwrap()
        .get(&lower)
        .expect("an attempt must be reserved after the first connect()");

    // Simulate the bookkeeping state left behind once a retry cycle is over
    // (backoff exhausted, or -- after the leak fix -- `retry_limit <= 1`):
    // `connect_request_attempt_ids` no longer has an entry for `lower`.
    t.connect_request_attempt_ids
        .write()
        .unwrap()
        .remove(&lower);

    assert!(
        t.connect(&lower).await.is_ok(),
        "connect() must succeed once the peer is still unconnected"
    );

    assert_eq!(
        signaler.0.lock().unwrap().len(),
        2,
        "a fresh connect() after the bookkeeping was cleared must send a new Request, \
         not be silently swallowed"
    );
    let second_attempt_id = *t
        .connect_request_attempt_ids
        .read()
        .unwrap()
        .get(&lower)
        .expect("a fresh attempt must be reserved for the restarted cycle");
    assert_ne!(
        first_attempt_id, second_attempt_id,
        "the restarted cycle must reserve a brand-new attempt id"
    );
}

#[tokio::test]
async fn higher_id_connect_keeps_pending_request_after_route_not_found() {
    use async_trait::async_trait;
    use mistlib_core::error::{MistError, Result as MistResult};
    use mistlib_core::signaling::Signaler;
    use std::sync::Arc;

    struct RouteMissingSignaler;

    #[async_trait]
    impl Signaler for RouteMissingSignaler {
        async fn send_signaling(&self, to: &NodeId, _msg: MessageContent) -> MistResult<()> {
            Err(MistError::RouteNotFound(to.clone()))
        }

        async fn close(&self) -> MistResult<()> {
            Ok(())
        }
    }

    let t = WebRtcTransport::new(Arc::new(RouteMissingSignaler), NodeId("zzz".to_string()));
    let lower = NodeId("aaa".to_string());

    assert!(t.connect(&lower).await.is_ok());
    assert_eq!(
        t.get_connection_state(&lower),
        ConnectionState::Disconnected
    );
    assert!(
        t.connect_request_attempt_ids
            .read()
            .unwrap()
            .contains_key(&lower),
        "failed direct Request must remain pending so the retry task can resend"
    );
}

#[tokio::test]
async fn incoming_offer_clears_pending_lower_id_request_retry() {
    use async_trait::async_trait;
    use mistlib_core::error::Result as MistResult;
    use mistlib_core::signaling::Signaler;
    use std::sync::{Arc, Mutex};

    struct RecordingSignaler(Mutex<Vec<(NodeId, MessageContent)>>);

    #[async_trait]
    impl Signaler for RecordingSignaler {
        async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> MistResult<()> {
            self.0.lock().unwrap().push((to.clone(), msg));
            Ok(())
        }

        async fn close(&self) -> MistResult<()> {
            Ok(())
        }
    }

    let signaler = Arc::new(RecordingSignaler(Mutex::new(Vec::new())));
    let t = WebRtcTransport::new(signaler, NodeId("zzz".to_string()));
    let lower = NodeId("aaa".to_string());

    assert!(t.connect(&lower).await.is_ok());
    assert!(t
        .connect_request_attempt_ids
        .read()
        .unwrap()
        .contains_key(&lower));

    let _ = t
        .handle_offer(lower.clone(), "invalid-offer".to_string())
        .await;

    assert!(
        !t.connect_request_attempt_ids
            .read()
            .unwrap()
            .contains_key(&lower),
        "offer arrival means the lower-id peer received our Request and started negotiation"
    );
}

#[tokio::test]
async fn connection_state_is_not_connected_after_failed_attempt() {
    let t = make_transport();
    let node = NodeId("unreachable".to_string());
    let _ = t.connect(&node).await;
    assert_ne!(
        t.get_connection_state(&node),
        ConnectionState::Connected,
        "Node should not be Connected after failed WebRTC setup"
    );
}

#[tokio::test]
async fn incoming_request_sequential_respects_max_limit() {
    use mistlib_core::signaling::SignalingHandler;
    let t = make_transport();
    t.set_max_connections(2);

    for i in 1..=3 {
        let msg = MessageContent::Data(SignalingData {
            sender_id: NodeId(format!("remote_{i}")),
            receiver_id: NodeId("local".to_string()),
            room_id: String::new(),
            signaling_type: SignalingType::Request,
            data: String::new(),
        });
        let _ = t.handle_message(msg).await;
    }

    let active = t.get_active_connection_states();
    assert_eq!(
        active.len(),
        2,
        "Even with 3 incoming requests, max capacity (2) must be strictly respected"
    );
}

#[tokio::test]
async fn incoming_request_stress_limit_with_wait() {
    use mistlib_core::signaling::SignalingHandler;
    use tokio::time::sleep;
    use web_time::Duration;

    let t = make_transport();
    const MAX: u32 = 6;
    t.set_max_connections(MAX);

    for i in 0..MAX {
        let msg = MessageContent::Data(SignalingData {
            sender_id: NodeId(format!("stress-remote-{}", i)),
            receiver_id: NodeId("local".to_string()),
            room_id: String::new(),
            signaling_type: SignalingType::Request,
            data: String::new(),
        });
        let _ = t.handle_message(msg).await;
    }

    sleep(Duration::from_millis(100)).await;

    let active = t.get_active_connection_states();
    assert_eq!(
        active.len(),
        MAX as usize,
        "Active connections must be strictly limited to {} even after waiting and 10 incoming requests",
        MAX
    );
}

/// tc-chat's browser sends a fresh Offer on the SAME peer connection when a
/// screen-share starts mid-session (mistlib-wasm's `publish_local_track` adds
/// an RTCRtpSender and renegotiates). `handle_offer` must apply that as
/// renegotiation on the existing live `Peer` -- not tear it down and build a
/// new RTCPeerConnection, which would kill the already-open data channels.
///
/// multi_thread required: see the reasoning on the disconnect-detection tests
/// in `disconnect.rs` -- A/B are independent peers and must not share an OS
/// thread for realistic scheduling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renegotiation_offer_on_existing_peer_is_applied_in_place() {
    use std::sync::Arc as StdArc;
    use webrtc::api::media_engine::MIME_TYPE_H264;
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
    use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

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

    // Snapshot B's Peer object identity for A before renegotiation.
    let peer_before = {
        let peers = tb.peers.read().await;
        peers
            .get(&id_a)
            .cloned()
            .expect("B should have a live peer for A")
    };

    // Simulate the browser side publishing a screen-share track mid-session:
    // A adds a local H264 track to the already-Connected peer and
    // renegotiates, sending a fresh Offer to B over the same signaling
    // channel/peer (mirrors `publish_local_track` + renegotiation in
    // mistlib-wasm).
    let track = StdArc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_string(),
        "stream".to_string(),
    ));
    ta.add_track_and_renegotiate(&id_b, track)
        .await
        .expect("renegotiation offer should succeed on an already-Connected peer");

    // Give the loopback signaling + answer roundtrip a moment to complete.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // B must still be Connected (not torn down into Connecting/Disconnected),
    // and it must be the SAME Peer object -- renegotiated in place, not
    // replaced by a freshly created RTCPeerConnection.
    assert_eq!(
        tb.get_connection_state(&id_a),
        ConnectionState::Connected,
        "B's connection to A must remain Connected across renegotiation"
    );
    let peer_after = {
        let peers = tb.peers.read().await;
        peers
            .get(&id_a)
            .cloned()
            .expect("B should still have a live peer for A after renegotiation")
    };
    assert!(
        StdArc::ptr_eq(&peer_before, &peer_after),
        "B's peer for A must be renegotiated in place, not replaced by a new RTCPeerConnection"
    );
}

/// Regression test for the field bug `Peer::negotiating` fixes: a browser
/// sending a track-publish offer immediately followed by a reconcile offer
/// (one more m-line) before the first offer's answer round-trips can, without
/// serializing `apply_offer` per peer, interleave two concurrent
/// `set_remote_description`/`create_answer`/`set_local_description` sequences
/// on the same `RTCPeerConnection`. Each call's tail reads whatever
/// `local_description()` currently holds, which is not necessarily the answer
/// *that* call just computed if the other call's `set_local_description`
/// landed first -- producing an answer whose m-line count/order doesn't match
/// the offer the far side thinks it just sent (Chrome: "The order of m-lines
/// in answer doesn't match order in offer").
///
/// This exercises the real dispatch shape that makes such overlap possible in
/// production: `MistEngine::handle_message_content`
/// (`mistlib-core/src/engine/network.rs`) spawns a brand-new, unserialized
/// task per inbound signaling message once a peer's connection is part of the
/// overlay mesh, rather than draining one sequential queue like the
/// WebSocket-bootstrap signaling path does. Two such offers for the same peer
/// handled concurrently, with nothing else ordering them, is exactly what
/// `Peer::negotiating` must serialize.
///
/// multi_thread required so the two `handle_offer` calls can genuinely race on
/// separate OS threads instead of only interleaving at `.await` points on one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_offers_on_same_peer_are_serialized_not_interleaved() {
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;
    use webrtc::api::media_engine::MIME_TYPE_OPUS;
    use webrtc::peer_connection::configuration::RTCConfiguration;
    use webrtc::peer_connection::signaling_state::RTCSignalingState;
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
    use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

    /// Records every Answer B sends, in whichever order the two concurrent
    /// `handle_offer` calls actually complete in.
    struct RecordingSignaler(StdMutex<Vec<String>>);

    #[async_trait]
    impl mistlib_core::signaling::Signaler for RecordingSignaler {
        async fn send_signaling(
            &self,
            _to: &NodeId,
            msg: MessageContent,
        ) -> mistlib_core::error::Result<()> {
            if let MessageContent::Data(d) = &msg {
                if d.signaling_type == SignalingType::Answer {
                    self.0.lock().unwrap().push(d.data.clone());
                }
            }
            Ok(())
        }

        async fn close(&self) -> mistlib_core::error::Result<()> {
            Ok(())
        }
    }

    fn count_m_lines(sdp: &str) -> usize {
        sdp.lines().filter(|line| line.starts_with("m=")).count()
    }

    let id_a = NodeId("peer-a".to_string());
    let id_b = NodeId("peer-b".to_string());

    let recorder = Arc::new(RecordingSignaler(StdMutex::new(Vec::new())));
    let tb = Arc::new(WebRtcTransport::new(
        recorder.clone() as Arc<dyn mistlib_core::signaling::Signaler>,
        id_b.clone(),
    ));

    // Give B a live, Stable peer for A directly -- what matters here is two
    // offers racing on an already-established peer, not how it got
    // established (a real two-way handshake reaching this same state is
    // exercised elsewhere, e.g.
    // `renegotiation_offer_on_existing_peer_is_applied_in_place`).
    let peer_b = tb
        .create_pc(id_a.clone())
        .await
        .expect("create_pc should succeed");
    tb.peers.write().await.insert(id_a.clone(), peer_b.clone());

    // Build two distinct, valid offers "from A" using B's own API/media-engine
    // config (`tb.api`) so B's `create_answer` can actually match codecs --
    // mirrors the browser sending offer B (adds a track) immediately followed
    // by offer C (adds another) without waiting for B's first answer.
    // `create_offer` never mutates signaling state (only
    // `set_local_description` does), so calling it twice on the same
    // throwaway PC with a track added in between is safe and needs no real
    // two-way handshake.
    let fake_a = tb
        .api
        .new_peer_connection(RTCConfiguration::default())
        .await
        .expect("throwaway peer connection should build");
    fake_a
        .create_data_channel("reliable", None)
        .await
        .expect("data channel should be created");
    let offer_1 = fake_a
        .create_offer(None)
        .await
        .expect("first offer should be created");

    let audio_track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            ..Default::default()
        },
        "audio".to_string(),
        "stream".to_string(),
    ));
    fake_a
        .add_track(audio_track)
        .await
        .expect("audio track should attach to the throwaway connection");
    let offer_2 = fake_a
        .create_offer(None)
        .await
        .expect("second offer should be created");

    assert_eq!(
        count_m_lines(&offer_1.sdp),
        1,
        "sanity: first offer is data-channel-only"
    );
    assert_eq!(
        count_m_lines(&offer_2.sdp),
        2,
        "sanity: second offer adds one more m-line"
    );

    // Fire both offers at B concurrently, with nothing serializing them from
    // the caller's side -- exactly the unserialized-dispatch shape described
    // above.
    let tb1 = tb.clone();
    let id_a1 = id_a.clone();
    let sdp_1 = offer_1.sdp.clone();
    let h1 = tokio::spawn(async move { tb1.handle_offer(id_a1, sdp_1).await });

    let tb2 = tb.clone();
    let id_a2 = id_a.clone();
    let sdp_2 = offer_2.sdp.clone();
    let h2 = tokio::spawn(async move { tb2.handle_offer(id_a2, sdp_2).await });

    let (r1, r2) = tokio::join!(h1, h2);
    r1.expect("task must not panic")
        .expect("first offer must be answered successfully");
    r2.expect("task must not panic")
        .expect("second offer must be answered successfully");

    assert_eq!(
        peer_b.pc.signaling_state(),
        RTCSignalingState::Stable,
        "peer must settle back to Stable after both renegotiations, not get wedged"
    );

    let answers = recorder.0.lock().unwrap().clone();
    assert_eq!(
        answers.len(),
        2,
        "exactly one answer must be sent per offer, none dropped or duplicated"
    );

    // The crux of the fix: each answer's m-line count must correspond to
    // exactly one of the two offers, not a mix produced by an interleaved
    // set_remote_description/create_answer/set_local_description sequence.
    let mut m_line_counts: Vec<usize> = answers.iter().map(|sdp| count_m_lines(sdp)).collect();
    m_line_counts.sort();
    assert_eq!(
        m_line_counts,
        vec![1, 2],
        "each answer must independently and correctly match exactly one of the two offers \
         (a corrupted/interleaved negotiation would instead produce two answers with the \
         same, wrong m-line count)"
    );
}

// --- Offer resend ---------------------------------------------------------
//
// Bounded initiator-side offer retransmission
// (`sweeper::spawn_offer_resend`, wired up from `connection::connect_inner`
// right after a fresh `send_offer` succeeds) plus the receiver-side
// idempotent duplicate-offer handling that makes a resend safe
// (`handle_offer`'s existing-peer path, `signaling::duplicate_offer_decision`).
// See the module-level constants in `webrtc.rs`
// (`OFFER_RESEND_SCHEDULE_MS`/`OFFER_RESEND_MAX`/`OFFER_RESEND_JITTER_MS`)
// for the schedule.

/// `duplicate_offer_decision`'s pure state table, exercised directly rather
/// than through a live handshake (the `HaveRemoteOffer` case in particular is
/// only a real, sub-millisecond window in production -- not something a test
/// should try to race against).
mod duplicate_offer_decision_tests {
    use crate::transports::webrtc::signaling::{duplicate_offer_decision, DuplicateOfferAction};
    use webrtc::peer_connection::signaling_state::RTCSignalingState;

    #[test]
    fn non_identical_sdp_never_resolves_regardless_of_state() {
        assert_eq!(
            duplicate_offer_decision(false, RTCSignalingState::Stable, true),
            None
        );
        assert_eq!(
            duplicate_offer_decision(false, RTCSignalingState::HaveRemoteOffer, false),
            None
        );
    }

    #[test]
    fn identical_sdp_stable_with_local_answer_resends_the_answer() {
        assert_eq!(
            duplicate_offer_decision(true, RTCSignalingState::Stable, true),
            Some(DuplicateOfferAction::ResendAnswer)
        );
    }

    #[test]
    fn identical_sdp_stable_without_local_answer_is_left_to_apply_offer() {
        // e.g. we are the initiator of this session (our own Offer, not an
        // Answer, is the local description) -- conservatively fall through
        // rather than guess.
        assert_eq!(
            duplicate_offer_decision(true, RTCSignalingState::Stable, false),
            None
        );
    }

    #[test]
    fn identical_sdp_have_remote_offer_is_ignored() {
        // The in-flight `apply_offer` call for the first delivery of this
        // same offer is still producing its answer on this same peer.
        assert_eq!(
            duplicate_offer_decision(true, RTCSignalingState::HaveRemoteOffer, false),
            Some(DuplicateOfferAction::Ignore)
        );
        assert_eq!(
            duplicate_offer_decision(true, RTCSignalingState::HaveRemoteOffer, true),
            Some(DuplicateOfferAction::Ignore)
        );
    }

    #[test]
    fn identical_sdp_have_local_offer_is_left_to_the_earlier_glare_check() {
        // `handle_offer`'s glare check runs (and, for the impolite side,
        // returns) before this decision is ever reached; this only proves
        // that if it doesn't fire, an identical `HaveLocalOffer` offer isn't
        // one of this decision's two recognized states either.
        assert_eq!(
            duplicate_offer_decision(true, RTCSignalingState::HaveLocalOffer, false),
            None
        );
    }
}

/// Receiver side: once a normal handshake has settled to `Stable` with our
/// answer on file, the initiator re-delivering the exact same Offer
/// byte-for-byte (`spawn_offer_resend`'s retransmission, or a duplicate for
/// any other reason) must be treated idempotently -- the existing answer is
/// re-sent verbatim, and `set_remote_description` must NOT run again,
/// observable as the connection staying `Connected`/`Stable` throughout
/// rather than risking the kind of negotiation error `Peer::negotiating`'s
/// doc comment describes.
///
/// multi_thread required: see the reasoning on the disconnect-detection
/// tests in `disconnect.rs` -- A/B are independent peers and must not share
/// an OS thread for realistic scheduling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_offer_after_stable_resends_the_existing_answer() {
    use std::sync::Mutex as StdMutex;
    use tokio::sync::mpsc;
    use webrtc::peer_connection::signaling_state::RTCSignalingState;

    /// Plain loopback forward, additionally counting every `Answer` message
    /// B ever sends -- distinguishes "B's already-produced answer stayed
    /// unchanged" (true regardless of this fix) from "B actually re-sent it"
    /// (the behavior this fix adds).
    struct CountingLoopbackSignaler {
        tx: mpsc::UnboundedSender<MessageContent>,
        answers_sent: Arc<StdMutex<usize>>,
    }

    #[async_trait::async_trait]
    impl mistlib_core::signaling::Signaler for CountingLoopbackSignaler {
        async fn send_signaling(
            &self,
            _to: &NodeId,
            msg: MessageContent,
        ) -> mistlib_core::error::Result<()> {
            if let MessageContent::Data(d) = &msg {
                if d.signaling_type == SignalingType::Answer {
                    *self.answers_sent.lock().unwrap() += 1;
                }
            }
            let _ = self.tx.send(msg);
            Ok(())
        }

        async fn close(&self) -> mistlib_core::error::Result<()> {
            Ok(())
        }
    }

    let id_a = NodeId("peer-a".to_string());
    let id_b = NodeId("peer-b".to_string());
    let (tx_a_to_b, rx_a_to_b) = mpsc::unbounded_channel::<MessageContent>();
    let (tx_b_to_a, rx_b_to_a) = mpsc::unbounded_channel::<MessageContent>();
    let answers_sent = Arc::new(StdMutex::new(0usize));

    let ta = Arc::new(WebRtcTransport::new(
        Arc::new(CountingLoopbackSignaler {
            tx: tx_a_to_b,
            answers_sent: Arc::new(StdMutex::new(0)),
        }),
        id_a.clone(),
    ));
    let tb = Arc::new(WebRtcTransport::new(
        Arc::new(CountingLoopbackSignaler {
            tx: tx_b_to_a,
            answers_sent: answers_sent.clone(),
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

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );
    assert_eq!(
        *answers_sent.lock().unwrap(),
        1,
        "sanity: exactly one answer sent during the normal handshake"
    );

    let peer_a_to_b = {
        let peers = ta.peers.read().await;
        peers
            .get(&id_b)
            .cloned()
            .expect("A should have a live peer for B")
    };
    let offer_sdp = peer_a_to_b
        .pc
        .local_description()
        .await
        .expect("A's offer should be set as its local description")
        .sdp;

    let peer_b_to_a = {
        let peers = tb.peers.read().await;
        peers
            .get(&id_a)
            .cloned()
            .expect("B should have a live peer for A")
    };
    assert_eq!(peer_b_to_a.pc.signaling_state(), RTCSignalingState::Stable);

    // Re-deliver the exact same offer B already answered.
    tb.handle_offer(id_a.clone(), offer_sdp.clone())
        .await
        .expect("a duplicate offer must not be treated as an error");

    // Give the loopback signaling roundtrip a moment.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert_eq!(
        peer_b_to_a.pc.signaling_state(),
        RTCSignalingState::Stable,
        "the duplicate offer must not disturb negotiation state (no re-run of \
         set_remote_description)"
    );
    assert_eq!(
        tb.get_connection_state(&id_a),
        ConnectionState::Connected,
        "B's connection to A must remain Connected across the duplicate offer"
    );
    assert_eq!(
        *answers_sent.lock().unwrap(),
        2,
        "the duplicate offer must trigger exactly one more Answer send -- the existing one, \
         re-sent verbatim"
    );
}

/// Initiator side, end-to-end: reproduces the measured production failure
/// mode this whole fix targets -- `send_signaling` returning `Ok` ("queued")
/// for the very first Offer, but the message never actually reaching the
/// wire (a churning overlay hop's send queue silently dropping it after
/// enqueue). Before this fix the initiator would sit in `HaveLocalOffer`
/// until the 6s connect watchdog killed the whole attempt; with it,
/// `spawn_offer_resend` retransmits the same, already-applied offer and the
/// handshake still completes.
///
/// multi_thread required: see the reasoning on the disconnect-detection
/// tests in `disconnect.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offer_resend_completes_the_handshake_after_the_first_offer_is_silently_lost() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::mpsc;

    /// Swallows exactly the first `SignalingType::Offer` message (returning
    /// `Ok`, matching the documented "queued but silently dropped
    /// downstream" failure mode -- NOT an error return, which a different,
    /// already-covered path handles) and counts every `Offer` it does
    /// forward, so this test can tell a resend actually happened.
    struct DropFirstOfferSignaler {
        tx: mpsc::UnboundedSender<MessageContent>,
        dropped_once: AtomicBool,
        offers_forwarded: Arc<StdMutex<usize>>,
    }

    #[async_trait::async_trait]
    impl mistlib_core::signaling::Signaler for DropFirstOfferSignaler {
        async fn send_signaling(
            &self,
            _to: &NodeId,
            msg: MessageContent,
        ) -> mistlib_core::error::Result<()> {
            if let MessageContent::Data(d) = &msg {
                if d.signaling_type == SignalingType::Offer {
                    if !self.dropped_once.swap(true, Ordering::SeqCst) {
                        return Ok(());
                    }
                    *self.offers_forwarded.lock().unwrap() += 1;
                }
            }
            let _ = self.tx.send(msg);
            Ok(())
        }

        async fn close(&self) -> mistlib_core::error::Result<()> {
            Ok(())
        }
    }

    /// Plain loopback forward for B -> A (B never needs any flakiness here).
    struct LoopbackSignaler {
        tx: mpsc::UnboundedSender<MessageContent>,
    }

    #[async_trait::async_trait]
    impl mistlib_core::signaling::Signaler for LoopbackSignaler {
        async fn send_signaling(
            &self,
            _to: &NodeId,
            msg: MessageContent,
        ) -> mistlib_core::error::Result<()> {
            let _ = self.tx.send(msg);
            Ok(())
        }

        async fn close(&self) -> mistlib_core::error::Result<()> {
            Ok(())
        }
    }

    // "peer-a" < "peer-b" lexically, so A is the deterministic initiator
    // (`WebRtcTransport::connect`'s `local_node_id.0 <= node.0` check) and
    // actually runs `connect_inner`/`send_offer`/`spawn_offer_resend`.
    let id_a = NodeId("peer-a".to_string());
    let id_b = NodeId("peer-b".to_string());
    let (tx_a_to_b, rx_a_to_b) = mpsc::unbounded_channel::<MessageContent>();
    let (tx_b_to_a, rx_b_to_a) = mpsc::unbounded_channel::<MessageContent>();
    let offers_forwarded = Arc::new(StdMutex::new(0usize));

    let ta = Arc::new(WebRtcTransport::new(
        Arc::new(DropFirstOfferSignaler {
            tx: tx_a_to_b,
            dropped_once: AtomicBool::new(false),
            offers_forwarded: offers_forwarded.clone(),
        }),
        id_a.clone(),
    ));
    let tb = Arc::new(WebRtcTransport::new(
        Arc::new(LoopbackSignaler { tx: tx_b_to_a }),
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

    ta.connect(&id_b).await.expect("connect should not fail");

    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A must still reach Connected -- the resend is the only thing that can deliver an \
         offer to B at all, since the very first one was silently dropped"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    // Give any further (should-not-happen) resends -- the schedule
    // (test values: 30ms/60ms + <=5ms jitter) -- a chance to also land,
    // proving the resend stopped once the offer was answered.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert_eq!(
        *offers_forwarded.lock().unwrap(),
        1,
        "exactly one resend must have reached B (the first Offer was dropped silently; \
         B's answer must have arrived before the second scheduled resend, since the \
         handshake completed) -- not zero (the fix wouldn't work at all) and not more than \
         one (a resend must stop once the offer is answered)"
    );
}
