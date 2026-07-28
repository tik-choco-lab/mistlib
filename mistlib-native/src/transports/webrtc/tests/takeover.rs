//! Tests for the stale-session remote-takeover fix (see the doc comment on
//! the `impl WebRtcTransport` block above `handle_offer` in `signaling.rs`):
//! Change 1 (`CONNECT_REQUEST` against a stale peer entry) and Change 2 (an
//! offer whose DTLS fingerprint differs from the existing session's), both
//! gated by the shared `takeover_allowed` guards in `webrtc.rs`.
//!
//! Layout mirrors the split called for in the spec: pure-function unit tests
//! for `sdp_fingerprint`, `takeover_allowed`, and `offer_takeover_decision`
//! first (exhaustive, no live `RTCPeerConnection` needed), then
//! integration-style tests exercising the real transport/signaling paths,
//! following the existing patterns in `tests/signaling.rs` and
//! `tests/disconnect.rs`.

use super::disconnect::{make_connected_pair, wait_for_state};
use super::*;
use crate::transports::webrtc::signaling::{offer_takeover_decision, sdp_fingerprint};
use crate::transports::webrtc::takeover_allowed;
use mistlib_core::signaling::{MessageContent, SignalingData, SignalingHandler, SignalingType};
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, NodeId};
use std::sync::Arc;

// --- `sdp_fingerprint` --------------------------------------------------

mod sdp_fingerprint_tests {
    use super::sdp_fingerprint;

    const SDP_WITH_FINGERPRINT: &str = "v=0\r\n\
         o=- 0 0 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         t=0 0\r\n\
         a=fingerprint:sha-256 AB:CD:EF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC\r\n";

    const SDP_WITHOUT_FINGERPRINT: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";

    #[test]
    fn extracts_fingerprint_when_present() {
        let fp = sdp_fingerprint(SDP_WITH_FINGERPRINT).expect("fingerprint should be found");
        assert_eq!(
            fp,
            "sha-256 AB:CD:EF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC"
        );
    }

    #[test]
    fn returns_none_when_no_fingerprint_line_is_present() {
        assert_eq!(sdp_fingerprint(SDP_WITHOUT_FINGERPRINT), None);
    }

    #[test]
    fn returns_none_for_an_empty_sdp() {
        assert_eq!(sdp_fingerprint(""), None);
    }

    #[test]
    fn normalizes_case_so_differently_cased_equivalents_compare_equal() {
        let lower = "a=fingerprint:SHA-256 ab:cd:ef:00\r\n";
        let upper = "a=fingerprint:sha-256 AB:CD:EF:00\r\n";
        let mixed = "a=fingerprint:Sha-256 aB:cD:eF:00\r\n";
        let fp_lower = sdp_fingerprint(lower).expect("lower should parse");
        let fp_upper = sdp_fingerprint(upper).expect("upper should parse");
        let fp_mixed = sdp_fingerprint(mixed).expect("mixed should parse");
        assert_eq!(fp_lower, fp_upper);
        assert_eq!(fp_lower, fp_mixed);
    }

    #[test]
    fn different_hex_digests_do_not_compare_equal() {
        let a = sdp_fingerprint("a=fingerprint:sha-256 AA:BB:CC\r\n").unwrap();
        let b = sdp_fingerprint("a=fingerprint:sha-256 AA:BB:CD\r\n").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn hash_algorithm_name_is_kept_in_the_result() {
        let fp = sdp_fingerprint("a=fingerprint:sha-1 AA:BB:CC\r\n").unwrap();
        assert!(
            fp.starts_with("sha-1 "),
            "the hash algorithm name must be part of the returned value: {fp}"
        );
    }

    #[test]
    fn a_line_with_no_hex_half_is_treated_as_missing() {
        assert_eq!(sdp_fingerprint("a=fingerprint:sha-256\r\n"), None);
    }

    #[test]
    fn finds_the_fingerprint_line_regardless_of_position() {
        let sdp = "v=0\r\ns=-\r\na=fingerprint:sha-256 AA:BB\r\na=ice-ufrag:xyz\r\n";
        assert_eq!(sdp_fingerprint(sdp), Some("sha-256 AA:BB".to_string()));
    }
}

// --- `takeover_allowed` (shared guards) ---------------------------------

mod takeover_allowed_tests {
    use super::takeover_allowed;
    use crate::transports::webrtc::{
        CONNECTION_TIMEOUT_MS, REMOTE_TAKEOVER_MIN_INTERVAL_MS, REMOTE_TAKEOVER_RECENT_CONNECT_MS,
    };

    #[test]
    fn allowed_with_no_prior_state_at_all() {
        assert!(takeover_allowed(false, None, None, None));
    }

    #[test]
    fn allowed_when_healthy_but_never_recorded_as_connected() {
        // `healthy` alone (no `ms_since_connected` entry) must not block --
        // the guard only fires when we can actually prove "recently".
        assert!(takeover_allowed(true, None, None, None));
    }

    #[test]
    fn blocked_when_healthy_and_recently_connected() {
        assert!(!takeover_allowed(true, Some(1), None, None));
    }

    #[test]
    fn unhealthy_session_gets_no_recent_connect_protection() {
        assert!(takeover_allowed(false, Some(1), None, None));
    }

    #[test]
    fn allowed_once_the_recent_connect_window_has_passed() {
        assert!(takeover_allowed(
            true,
            Some(REMOTE_TAKEOVER_RECENT_CONNECT_MS + 1),
            None,
            None
        ));
    }

    #[test]
    fn boundary_ms_since_connected_exactly_at_threshold_is_not_recent() {
        // Guard condition is strict `<`, so `ms == threshold` no longer
        // counts as "recent".
        assert!(takeover_allowed(
            true,
            Some(REMOTE_TAKEOVER_RECENT_CONNECT_MS),
            None,
            None
        ));
    }

    #[test]
    fn blocked_when_within_the_rate_limit_window_even_if_unhealthy_and_never_connected() {
        assert!(!takeover_allowed(false, None, Some(1), None));
    }

    #[test]
    fn allowed_once_the_rate_limit_window_has_passed() {
        assert!(takeover_allowed(
            false,
            None,
            Some(REMOTE_TAKEOVER_MIN_INTERVAL_MS + 1),
            None
        ));
    }

    #[test]
    fn boundary_ms_since_last_takeover_exactly_at_threshold_is_not_rate_limited() {
        assert!(takeover_allowed(
            false,
            None,
            Some(REMOTE_TAKEOVER_MIN_INTERVAL_MS),
            None
        ));
    }

    #[test]
    fn both_guards_combine_to_block() {
        assert!(!takeover_allowed(true, Some(1), Some(1), None));
    }

    #[test]
    fn rate_limit_blocks_even_when_the_recent_connect_guard_would_allow() {
        assert!(!takeover_allowed(
            true,
            Some(REMOTE_TAKEOVER_RECENT_CONNECT_MS + 1),
            Some(1),
            None
        ));
    }

    // --- Fix B: young in-flight attempt guard ---------------------------

    #[test]
    fn blocked_when_an_in_flight_attempt_is_young_even_if_otherwise_unhealthy_and_never_connected()
    {
        // This is the exact shape of the measured regression: an unhealthy
        // (mid-handshake, never-yet-`Connected`) session with a young
        // `connect_started_at` entry must not be taken over just because
        // neither of the other two guards fires.
        assert!(!takeover_allowed(false, None, None, Some(1)));
    }

    #[test]
    fn allowed_once_the_in_flight_attempt_window_has_passed() {
        assert!(takeover_allowed(
            false,
            None,
            None,
            Some(CONNECTION_TIMEOUT_MS as u128 + 1)
        ));
    }

    #[test]
    fn boundary_ms_since_connect_started_exactly_at_threshold_is_not_young() {
        // Strict `<`, matching the recent-connect guard's own boundary
        // convention: `ms == threshold` no longer counts as "young".
        assert!(takeover_allowed(
            false,
            None,
            None,
            Some(CONNECTION_TIMEOUT_MS as u128)
        ));
    }

    #[test]
    fn no_connect_started_entry_gets_no_in_flight_attempt_protection() {
        // `None` means "no attempt currently in flight" -- nothing for this
        // guard to protect, same convention as every other `ms_since_*`
        // input.
        assert!(takeover_allowed(false, None, None, None));
    }

    #[test]
    fn young_in_flight_attempt_guard_combines_with_the_other_two() {
        assert!(!takeover_allowed(true, Some(1), Some(1), Some(1)));
    }
}

// --- `offer_takeover_decision` (Change 2's fingerprint x guards table) --

mod offer_takeover_decision_tests {
    use super::offer_takeover_decision;

    #[test]
    fn same_fingerprint_never_takes_over_even_if_guards_would_allow_it() {
        assert!(!offer_takeover_decision(
            Some("sha-256 AA"),
            Some("sha-256 AA"),
            false,
            None,
            None,
            None
        ));
    }

    #[test]
    fn missing_existing_fingerprint_keeps_todays_behavior() {
        assert!(!offer_takeover_decision(
            None,
            Some("sha-256 AA"),
            false,
            None,
            None,
            None
        ));
    }

    #[test]
    fn missing_incoming_fingerprint_keeps_todays_behavior() {
        assert!(!offer_takeover_decision(
            Some("sha-256 AA"),
            None,
            false,
            None,
            None,
            None
        ));
    }

    #[test]
    fn both_fingerprints_missing_keeps_todays_behavior() {
        assert!(!offer_takeover_decision(
            None, None, false, None, None, None
        ));
    }

    #[test]
    fn different_fingerprints_take_over_when_guards_allow() {
        assert!(offer_takeover_decision(
            Some("sha-256 AA"),
            Some("sha-256 BB"),
            false,
            None,
            None,
            None
        ));
    }

    #[test]
    fn different_fingerprints_do_not_take_over_when_recently_connected_and_healthy() {
        assert!(!offer_takeover_decision(
            Some("sha-256 AA"),
            Some("sha-256 BB"),
            true,
            Some(1),
            None,
            None
        ));
    }

    #[test]
    fn different_fingerprints_do_not_take_over_when_rate_limited() {
        assert!(!offer_takeover_decision(
            Some("sha-256 AA"),
            Some("sha-256 BB"),
            false,
            None,
            Some(1),
            None
        ));
    }

    #[test]
    fn different_fingerprints_do_not_take_over_when_an_in_flight_attempt_is_young() {
        // Fix B: the fingerprint-mismatch path shares `takeover_allowed`'s
        // guards with the CONNECT_REQUEST path -- a young in-flight attempt
        // must block this takeover too.
        assert!(!offer_takeover_decision(
            Some("sha-256 AA"),
            Some("sha-256 BB"),
            false,
            None,
            None,
            Some(1)
        ));
    }
}

// --- Integration-style tests ---------------------------------------------

/// Change 1: reproduces the "mirror silent" failure mode described in the
/// spec -- `t` (the deterministic offerer for `sender`, i.e. sorts lower)
/// still has a `self.peers` entry for `sender`, so a `CONNECT_REQUEST` from
/// `sender` used to hit `Transport::connect`'s `if peers.contains_key(node)
/// { return Ok(()); }` fast path and be silently swallowed. With the fix,
/// the stale entry is unhealthy (no real handshake ever completed on it) and
/// there's no prior takeover recorded, so the guards allow it: the entry
/// must be replaced and a fresh Offer actually sent.
#[tokio::test]
async fn connect_request_takes_over_a_stale_peer_entry_and_sends_a_fresh_offer() {
    use async_trait::async_trait;
    use mistlib_core::error::Result as MistResult;
    use mistlib_core::signaling::Signaler;
    use std::sync::Mutex;

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
    let t = WebRtcTransport::new(signaler.clone(), NodeId("aaa".to_string()));
    let sender = NodeId("zzz".to_string());

    // A stale peer entry: `t` still thinks it has a session with `sender`,
    // but the underlying RTCPeerConnection never actually finished
    // connecting (no real network involved in this test, but the important
    // property -- unhealthy, i.e. not `Connected` with an open ReliableOrdered
    // data channel -- matches a genuinely dead session just as well).
    let stale_peer = t
        .create_pc(sender.clone())
        .await
        .expect("stale peer connection should be created");
    t.peers
        .write()
        .await
        .insert(sender.clone(), stale_peer.clone());
    t.connection_states
        .write()
        .unwrap()
        .insert(sender.clone(), ConnectionState::Connected);

    let msg = MessageContent::Data(SignalingData {
        sender_id: sender.clone(),
        receiver_id: t.local_node_id.clone(),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: String::new(),
    });

    assert!(t.handle_message(msg).await.is_ok());

    let offers_sent = {
        let sent = signaler.0.lock().unwrap();
        sent.iter()
            .filter(|(_, msg)| {
                matches!(msg, MessageContent::Data(d) if d.signaling_type == SignalingType::Offer)
            })
            .count()
    };
    assert_eq!(
        offers_sent, 1,
        "today's behavior (peers.contains_key short-circuit in Transport::connect) would \
         silently swallow the CONNECT_REQUEST and send nothing; the fix must tear the stale \
         entry down and send a fresh offer instead"
    );

    let current_peer = t.peers.read().await.get(&sender).cloned();
    assert!(
        current_peer.is_some_and(|p| !Arc::ptr_eq(&p, &stale_peer)),
        "the stale peer entry must be replaced by a brand-new Peer, not silently reused"
    );
}

/// Change 1's recent-connect guard: a `CONNECT_REQUEST` arriving while a
/// session is both healthy (`Connected`, ReliableOrdered data channel open)
/// and was established less than `REMOTE_TAKEOVER_RECENT_CONNECT_MS` ago
/// must leave that session completely untouched -- this is exactly the
/// "attempt that already succeeded" case the guard exists to protect,
/// distinguishing it from the genuinely-stale-entry case exercised above.
///
/// multi_thread required: see the reasoning on the disconnect-detection
/// tests in `disconnect.rs` -- A/B are independent peers and must not share
/// an OS thread for realistic scheduling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_request_within_the_recent_connect_window_leaves_a_healthy_session_untouched() {
    let (ta, tb, id_a, id_b) = make_connected_pair();

    // `id_a` ("peer-a") sorts lower than `id_b` ("peer-b"), so A is the
    // deterministic offerer for a Request from B -- exactly the shape Change
    // 1 acts on.
    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    let peer_before = {
        let peers = ta.peers.read().await;
        peers
            .get(&id_b)
            .cloned()
            .expect("A should have a live peer for B")
    };

    let msg = MessageContent::Data(SignalingData {
        sender_id: id_b.clone(),
        receiver_id: id_a.clone(),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: String::new(),
    });
    assert!(ta.handle_message(msg).await.is_ok());

    let peer_after = {
        let peers = ta.peers.read().await;
        peers
            .get(&id_b)
            .cloned()
            .expect("A must still have a live peer for B")
    };
    assert!(
        Arc::ptr_eq(&peer_before, &peer_after),
        "a recent, healthy connection must not be torn down by a CONNECT_REQUEST arriving \
         within the recent-connect guard window"
    );
    assert_eq!(
        ta.get_connection_state(&id_b),
        ConnectionState::Connected,
        "the healthy session must remain Connected"
    );
}

/// Change 2: an inbound offer for an already-known peer whose DTLS
/// fingerprint differs from the existing (never-actually-connected, hence
/// unhealthy) session's must tear that session down and answer from a fresh
/// `RTCPeerConnection` -- reproducing the "mirror still connected" failure
/// mode instead of reusing the dead PC.
///
/// Driving two distinct fake senders (rather than a real two-way handshake)
/// mirrors `signaling::concurrent_offers_on_same_peer_are_serialized_not_interleaved`'s
/// approach: `create_offer` on a throwaway `RTCPeerConnection` never
/// requires real ICE/network, and each fresh `RTCPeerConnection` gets its
/// own DTLS certificate (hence a different fingerprint) for free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_fingerprint_offer_against_an_existing_peer_takes_over_and_answers_fresh() {
    use async_trait::async_trait;
    use mistlib_core::error::Result as MistResult;
    use mistlib_core::signaling::Signaler;
    use std::sync::Mutex;
    use webrtc::peer_connection::configuration::RTCConfiguration;

    struct RecordingSignaler(Mutex<Vec<MessageContent>>);

    #[async_trait]
    impl Signaler for RecordingSignaler {
        async fn send_signaling(&self, _to: &NodeId, msg: MessageContent) -> MistResult<()> {
            self.0.lock().unwrap().push(msg);
            Ok(())
        }

        async fn close(&self) -> MistResult<()> {
            Ok(())
        }
    }

    let id_a = NodeId("peer-a".to_string());
    let id_b = NodeId("peer-b".to_string());

    let recorder = Arc::new(RecordingSignaler(Mutex::new(Vec::new())));
    let tb = Arc::new(WebRtcTransport::new(
        recorder.clone() as Arc<dyn mistlib_core::signaling::Signaler>,
        id_b.clone(),
    ));

    async fn build_offer(tb: &WebRtcTransport) -> String {
        let fake_a = tb
            .api
            .new_peer_connection(RTCConfiguration::default())
            .await
            .expect("throwaway peer connection should build");
        fake_a
            .create_data_channel("reliable", None)
            .await
            .expect("data channel should be created");
        fake_a
            .create_offer(None)
            .await
            .expect("offer should be created")
            .sdp
    }

    let offer_1 = build_offer(&tb).await;
    let offer_2 = build_offer(&tb).await;
    let fp_1 = sdp_fingerprint(&offer_1).expect("offer 1 must carry a DTLS fingerprint");
    let fp_2 = sdp_fingerprint(&offer_2).expect("offer 2 must carry a DTLS fingerprint");
    assert_ne!(
        fp_1, fp_2,
        "sanity: two independently-created RTCPeerConnections must have distinct certificates"
    );

    tb.handle_offer(id_a.clone(), offer_1)
        .await
        .expect("first offer (brand-new peer path) must be answered");
    let peer_1 = {
        let peers = tb.peers.read().await;
        peers
            .get(&id_a)
            .cloned()
            .expect("B should have a peer for A after the first offer")
    };

    // This synthetic test never lets a real data channel open (both offers
    // come from throwaway `RTCPeerConnection`s with no real ICE/DTLS
    // continuing on the other end), so the first offer's answer-side
    // `connect_started_at` entry -- normally cleared once the ReliableOrdered
    // DC opens or the connect watchdog times it out -- would otherwise still
    // look "young" to Fix B's in-flight-attempt guard when the second offer
    // arrives a few milliseconds later, blocking the very takeover this test
    // targets. Clear it explicitly to represent the passage of time a real
    // deployment would have here, so this test keeps exercising its actual
    // subject: a stale session with no young attempt still gets taken over
    // on a differing fingerprint (see the Fix B tests in this file for the
    // young-attempt case itself).
    tb.connect_started_at.write().unwrap().remove(&id_a);

    tb.handle_offer(id_a.clone(), offer_2)
        .await
        .expect("second, different-fingerprint offer must still be answered (from a fresh PC)");
    let peer_2 = {
        let peers = tb.peers.read().await;
        peers
            .get(&id_a)
            .cloned()
            .expect("B should have a peer for A after the takeover")
    };

    assert!(
        !Arc::ptr_eq(&peer_1, &peer_2),
        "a different-fingerprint offer must take over: the old session's Peer must be replaced, \
         not reused (which is what applying it to the dead PC via apply_offer would have done)"
    );

    let answers_sent = recorder
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|msg| matches!(msg, MessageContent::Data(d) if d.signaling_type == SignalingType::Answer))
        .count();
    assert_eq!(
        answers_sent, 2,
        "both the original offer and the takeover offer must each be answered exactly once"
    );
}

/// Change 2's same-fingerprint path: a renegotiation offer built on the SAME
/// `RTCPeerConnection` (and therefore the same DTLS certificate) as the
/// existing session must compare equal via `sdp_fingerprint`, which is what
/// keeps `handle_offer` on today's apply-in-place path instead of taking
/// over. `signaling::renegotiation_offer_on_existing_peer_is_applied_in_place`
/// already asserts the end-to-end behavior (peer identity preserved across a
/// real renegotiation); this test asserts the fingerprint-equality mechanism
/// that decision now depends on, directly against a real two-way handshake.
///
/// multi_thread required: see the reasoning on the disconnect-detection
/// tests in `disconnect.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_fingerprint_is_produced_by_a_single_peer_connections_offer_and_remote_view() {
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

    let peer_a_for_b = {
        let peers = ta.peers.read().await;
        peers
            .get(&id_b)
            .cloned()
            .expect("A should have a peer for B")
    };
    let peer_b_for_a = {
        let peers = tb.peers.read().await;
        peers
            .get(&id_a)
            .cloned()
            .expect("B should have a peer for A")
    };

    let a_local_fp = sdp_fingerprint(
        &peer_a_for_b
            .pc
            .local_description()
            .await
            .expect("A's local description should be set")
            .sdp,
    );
    let b_remote_fp = sdp_fingerprint(
        &peer_b_for_a
            .pc
            .remote_description()
            .await
            .expect("B's remote description should be set")
            .sdp,
    );

    assert!(
        a_local_fp.is_some(),
        "A's offer must carry a DTLS fingerprint"
    );
    assert_eq!(
        a_local_fp, b_remote_fp,
        "A's own offer/local description and B's view of A's remote description must carry the \
         same DTLS fingerprint -- both come from A's single, unchanging RTCPeerConnection, which \
         is exactly why a later renegotiation from A must not be mistaken for a fresh peer"
    );
}

/// The per-peer rate limit: a second takeover attempt for the same peer,
/// arriving immediately after the first, must be refused even though the
/// peer entry it would act on (the one `connect()` just installed as part of
/// completing the first takeover) still isn't a genuinely healthy session --
/// only the rate limit stands between the two attempts here.
#[tokio::test]
async fn second_takeover_within_the_rate_limit_window_is_refused() {
    use async_trait::async_trait;
    use mistlib_core::error::Result as MistResult;
    use mistlib_core::signaling::Signaler;
    use std::sync::Mutex;

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
    let t = WebRtcTransport::new(signaler, NodeId("aaa".to_string()));
    let sender = NodeId("zzz".to_string());

    let stale_peer = t
        .create_pc(sender.clone())
        .await
        .expect("stale peer connection should be created");
    t.peers
        .write()
        .await
        .insert(sender.clone(), stale_peer.clone());
    t.connection_states
        .write()
        .unwrap()
        .insert(sender.clone(), ConnectionState::Connected);

    let request = || {
        MessageContent::Data(SignalingData {
            sender_id: sender.clone(),
            receiver_id: NodeId("aaa".to_string()),
            room_id: String::new(),
            signaling_type: SignalingType::Request,
            data: String::new(),
        })
    };

    // First Request: takes over the stale entry (unhealthy, no prior
    // takeover recorded -- both guards allow it).
    assert!(t.handle_message(request()).await.is_ok());
    let peer_after_first_takeover = t
        .peers
        .read()
        .await
        .get(&sender)
        .cloned()
        .expect("connect() should install a fresh peer as part of completing the takeover");
    assert!(
        !Arc::ptr_eq(&stale_peer, &peer_after_first_takeover),
        "sanity: the first Request must take over the stale entry"
    );
    assert!(
        t.last_takeover_at.read().unwrap().contains_key(&sender),
        "a completed takeover must be recorded for the rate limit"
    );

    // Second Request, immediately after: the peer entry installed by the
    // first takeover's `connect()` still isn't genuinely healthy (no real
    // handshake completed), so only the per-peer rate limit
    // (`REMOTE_TAKEOVER_MIN_INTERVAL_MS`) can be what refuses this one.
    assert!(t.handle_message(request()).await.is_ok());
    let peer_after_second_request = t
        .peers
        .read()
        .await
        .get(&sender)
        .cloned()
        .expect("the peer entry must still exist");
    assert!(
        Arc::ptr_eq(&peer_after_first_takeover, &peer_after_second_request),
        "a second takeover within REMOTE_TAKEOVER_MIN_INTERVAL_MS must be refused by the \
         per-peer rate limit, leaving the peer installed by the first takeover untouched"
    );
}

/// Fix B: the chronic-thrashing regression measured on a steady 50-node
/// fleet with no fault injection -- 1209 `remote_connect_request_takeover`s
/// per 30min, 537 of them fused into a `connect_inner_error` within 300ms.
/// `t` ("aaa", the deterministic offerer) has already started its own dial
/// to `sender` ("zzz") -- a young `connect_started_at` entry, exactly what
/// `connect_inner` records right after acquiring its handshake permit -- when
/// a `CONNECT_REQUEST` from `sender` arrives (the higher-ID side's
/// `CONNECT_REQUEST_RETRY_INITIAL_MS` 1s nudge landing squarely inside the
/// dial, since fresh cross-host handshakes take >1s at p90 under load). This
/// must NOT tear the in-flight attempt down: with the guard, the request is
/// just a harmless no-op nudge (mirroring `Transport::connect`'s own
/// `peers.contains_key` fast path, since `t` still has a peer for `sender`).
#[tokio::test]
async fn connect_request_does_not_take_over_a_young_in_flight_attempt() {
    use async_trait::async_trait;
    use mistlib_core::error::Result as MistResult;
    use mistlib_core::signaling::Signaler;
    use std::sync::Mutex;

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
    let t = WebRtcTransport::new(signaler.clone(), NodeId("aaa".to_string()));
    let sender = NodeId("zzz".to_string());

    let in_flight_peer = t
        .create_pc(sender.clone())
        .await
        .expect("in-flight peer connection should be created");
    t.peers
        .write()
        .await
        .insert(sender.clone(), in_flight_peer.clone());
    t.connection_states
        .write()
        .unwrap()
        .insert(sender.clone(), ConnectionState::Connecting);
    // Simulate a dial to `sender` that only just started -- well within
    // CONNECTION_TIMEOUT_MS -- exactly what `connect_inner` records right
    // after acquiring its handshake permit.
    t.connect_started_at
        .write()
        .unwrap()
        .insert(sender.clone(), std::time::Instant::now());

    let msg = MessageContent::Data(SignalingData {
        sender_id: sender.clone(),
        receiver_id: t.local_node_id.clone(),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: String::new(),
    });

    assert!(t.handle_message(msg).await.is_ok());

    let offers_sent = {
        let sent = signaler.0.lock().unwrap();
        sent.iter()
            .filter(|(_, msg)| {
                matches!(msg, MessageContent::Data(d) if d.signaling_type == SignalingType::Offer)
            })
            .count()
    };
    assert_eq!(
        offers_sent, 0,
        "a young in-flight attempt must not be disturbed -- no fresh offer should be sent"
    );

    let current_peer = t.peers.read().await.get(&sender).cloned();
    assert!(
        current_peer.is_some_and(|p| Arc::ptr_eq(&p, &in_flight_peer)),
        "the young in-flight attempt's peer must survive the racing CONNECT_REQUEST untouched"
    );
    assert!(
        !t.last_takeover_at.read().unwrap().contains_key(&sender),
        "refusing on the young-in-flight-attempt guard must not be recorded as a takeover"
    );
}
