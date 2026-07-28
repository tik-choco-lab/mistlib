use super::*;
use mistlib_core::types::{ConnectionState, NodeId};
use std::time::{Duration, Instant};

#[test]
fn push_pending_candidate_does_not_evict_under_the_cap() {
    let mut list = Vec::new();
    for i in 0..MAX_PENDING_CANDIDATES_PER_NODE {
        let dropped = push_pending_candidate(&mut list, format!("cand-{i}"));
        assert!(!dropped, "must not evict while at or under the cap");
    }
    assert_eq!(list.len(), MAX_PENDING_CANDIDATES_PER_NODE);
}

#[test]
fn push_pending_candidate_evicts_oldest_past_the_cap() {
    let mut list = Vec::new();
    for i in 0..MAX_PENDING_CANDIDATES_PER_NODE {
        push_pending_candidate(&mut list, format!("cand-{i}"));
    }

    let dropped = push_pending_candidate(&mut list, "cand-overflow".to_string());

    assert!(dropped, "pushing past the cap must report an eviction");
    assert_eq!(
        list.len(),
        MAX_PENDING_CANDIDATES_PER_NODE,
        "list must stay bounded at the cap"
    );
    assert_eq!(
        list.first().map(String::as_str),
        Some("cand-1"),
        "the oldest entry (cand-0) must be the one evicted"
    );
    assert_eq!(
        list.last().map(String::as_str),
        Some("cand-overflow"),
        "the newest entry must be kept"
    );
}

#[tokio::test]
async fn handle_candidate_buffers_are_bounded_for_an_active_node() {
    let t = make_transport();
    let node = NodeId("peer".to_string());
    t.connection_states
        .write()
        .unwrap()
        .insert(node.clone(), ConnectionState::Connecting);

    for i in 0..(MAX_PENDING_CANDIDATES_PER_NODE + 10) {
        t.handle_candidate(node.clone(), format!("cand-{i}"))
            .await
            .expect("buffering a candidate for an active node must not error");
    }

    let pending = t.pending_candidates.read().await;
    let list = pending
        .get(&node)
        .expect("node must have buffered candidates");
    assert_eq!(
        list.len(),
        MAX_PENDING_CANDIDATES_PER_NODE,
        "buffered candidates for a single node must stay bounded at the cap"
    );
}

// --- Fix C: buffer, don't drop, candidates for an unknown node -----------

/// The total-map bound: once `pending_candidates` already tracks
/// `MAX_PENDING_CANDIDATE_NODES` distinct (never-reserved) nodes, a
/// candidate for one more brand-new node must be refused outright rather
/// than silently growing the map past the cap -- see
/// `MAX_PENDING_CANDIDATE_NODES`'s doc comment.
#[tokio::test]
async fn handle_candidate_refuses_a_brand_new_node_once_the_total_node_bound_is_hit() {
    let t = make_transport();

    for i in 0..MAX_PENDING_CANDIDATE_NODES {
        let node = NodeId(format!("unreserved-node-{i}"));
        t.handle_candidate(node, "candidate".to_string())
            .await
            .expect("buffering up to the total-node bound must not error");
    }
    assert_eq!(
        t.pending_candidates.read().await.len(),
        MAX_PENDING_CANDIDATE_NODES,
        "test setup: must be sitting exactly at the total-node bound"
    );

    let overflow_node = NodeId("one-node-too-many".to_string());
    t.handle_candidate(overflow_node.clone(), "candidate".to_string())
        .await
        .expect("refusing a new node's buffer must not itself be an error");

    assert_eq!(
        t.pending_candidates.read().await.len(),
        MAX_PENDING_CANDIDATE_NODES,
        "the total-node bound must not be exceeded"
    );
    assert!(
        !t.pending_candidates
            .read()
            .await
            .contains_key(&overflow_node),
        "the node that pushed past the bound must not be buffered at all"
    );

    // Existing nodes' buffers must be untouched by the refusal (no eviction
    // of another node's buffer -- see the constant's doc comment for why
    // this implementation refuses the new insert instead).
    let still_present = t
        .pending_candidates
        .read()
        .await
        .contains_key(&NodeId("unreserved-node-0".to_string()));
    assert!(
        still_present,
        "refusing a new node's buffer must not evict an existing node's buffer"
    );
}

/// A candidate that arrives before the offer that would reserve its node
/// must be buffered, and then actually applied once that offer arrives --
/// exercising the answer side (`handle_offer` -> `apply_offer`) of the drain
/// path for a node that started out completely unknown, not merely
/// `Connecting` with no peer yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidate_buffered_for_a_previously_unknown_node_is_applied_once_the_offer_arrives() {
    use webrtc::peer_connection::configuration::RTCConfiguration;

    let t = make_transport();
    let node = NodeId("candidate-before-offer-peer".to_string());

    // The trickled Candidate arrives first: `node` has no `peers` entry and
    // no `connection_states` reservation at all yet.
    t.handle_candidate(
        node.clone(),
        "not-a-real-candidate-but-parsing-happens-later".to_string(),
    )
    .await
    .expect("buffering for a previously unknown node must not error");
    assert!(
        t.pending_candidates.read().await.contains_key(&node),
        "test setup: the candidate must have been buffered"
    );
    assert!(
        t.pending_candidates_first_seen
            .read()
            .await
            .contains_key(&node),
        "test setup: the first-seen timestamp must have been recorded"
    );

    // Now the offer that reserves `node` arrives -- built on a throwaway
    // PeerConnection, mirroring `takeover.rs`'s `build_offer` helper (no
    // real network needed just to produce a valid SDP offer string).
    let fake_remote = t
        .api
        .new_peer_connection(RTCConfiguration::default())
        .await
        .expect("throwaway peer connection should build");
    fake_remote
        .create_data_channel("reliable", None)
        .await
        .expect("data channel should be created");
    let offer_sdp = fake_remote
        .create_offer(None)
        .await
        .expect("offer should be created")
        .sdp;

    t.handle_offer(node.clone(), offer_sdp)
        .await
        .expect("the offer for the now-known node must be answered");

    assert!(
        !t.pending_candidates.read().await.contains_key(&node),
        "the buffered candidate must be drained once the offer sets a remote description"
    );
    assert!(
        !t.pending_candidates_first_seen
            .read()
            .await
            .contains_key(&node),
        "the first-seen timestamp must be cleared alongside the drained buffer"
    );
    assert!(
        t.peers.read().await.contains_key(&node),
        "the offer must still have created a live peer for the now-known node"
    );
}

/// Fix C item 3: a node whose candidates were buffered but which never went
/// on to get a `connection_states` reservation (no offer/answer ever
/// arrived) must have its buffer aged out by the sweeper instead of sitting
/// in `pending_candidates` forever.
#[tokio::test]
async fn sweeper_discards_unreserved_pending_candidates_past_the_ttl() {
    use crate::transports::webrtc::PENDING_CANDIDATE_UNRESERVED_TTL_MS;

    let t = make_transport();
    let node = NodeId("abandoned-unreserved-buffer-peer".to_string());

    t.handle_candidate(node.clone(), "candidate".to_string())
        .await
        .expect("buffering should not error");
    // Backdate the first-seen timestamp past the TTL -- equivalent to the
    // offer/answer that would have reserved this node simply never arriving.
    t.pending_candidates_first_seen.write().await.insert(
        node.clone(),
        Instant::now() - Duration::from_millis(PENDING_CANDIDATE_UNRESERVED_TTL_MS + 1),
    );

    t.ensure_session_sweeper();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !t.pending_candidates.read().await.contains_key(&node) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sweeper should discard the stale unreserved candidate buffer");
    t.stop_session_sweeper();

    assert!(
        !t.pending_candidates_first_seen
            .read()
            .await
            .contains_key(&node),
        "the first-seen timestamp must be cleared alongside the discarded buffer"
    );
}

/// Counterpart to the TTL test above: a fresh unreserved buffer (well within
/// the TTL) must survive the sweeper untouched.
#[tokio::test]
async fn sweeper_does_not_discard_a_fresh_unreserved_pending_candidate_buffer() {
    use crate::transports::webrtc::PENDING_CANDIDATE_UNRESERVED_TTL_MS;

    let t = make_transport();
    let node = NodeId("fresh-unreserved-buffer-peer".to_string());

    t.handle_candidate(node.clone(), "candidate".to_string())
        .await
        .expect("buffering should not error");

    t.ensure_session_sweeper();
    tokio::time::sleep(Duration::from_millis(
        PENDING_CANDIDATE_UNRESERVED_TTL_MS / 2,
    ))
    .await;
    t.stop_session_sweeper();

    assert!(
        t.pending_candidates.read().await.contains_key(&node),
        "a buffer younger than the TTL must survive the sweeper"
    );
}
