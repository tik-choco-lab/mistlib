#[path = "../src/transport/webrtc/pending_candidates.rs"]
mod pending_candidates;

use mistlib_core::types::{ConnectionState, NodeId};
use pending_candidates::{
    is_active_for_pending, PendingCandidates, MAX_PENDING_CANDIDATES_PER_NODE,
};

#[test]
fn pending_candidates_push_take_and_remove() {
    let node = NodeId("node-a".to_string());
    let mut pending = PendingCandidates::default();

    assert!(!pending.push(node.clone(), "cand-1".to_string()));
    assert!(!pending.push(node.clone(), "cand-2".to_string()));
    assert_eq!(pending.len_for(&node), 2);

    let candidates = pending.take(&node).expect("pending candidates");
    assert_eq!(candidates, vec!["cand-1".to_string(), "cand-2".to_string()]);
    assert_eq!(pending.len_for(&node), 0);

    assert!(!pending.push(node.clone(), "cand-3".to_string()));
    assert_eq!(pending.remove(&node), Some(vec!["cand-3".to_string()]));
    assert_eq!(pending.len_for(&node), 0);

    assert!(!pending.push(node.clone(), "cand-4".to_string()));
    pending.clear();
    assert_eq!(pending.len_for(&node), 0);
}

#[test]
fn pending_candidates_keeps_newest_when_limit_is_exceeded() {
    let node = NodeId("node-a".to_string());
    let mut pending = PendingCandidates::default();

    for i in 0..MAX_PENDING_CANDIDATES_PER_NODE {
        assert!(!pending.push(node.clone(), format!("cand-{i}")));
    }
    assert!(pending.push(node.clone(), "cand-new".to_string()));

    let candidates = pending.take(&node).expect("pending candidates");
    assert_eq!(candidates.len(), MAX_PENDING_CANDIDATES_PER_NODE);
    assert_eq!(candidates.first().map(String::as_str), Some("cand-1"));
    assert_eq!(candidates.last().map(String::as_str), Some("cand-new"));
}

#[test]
fn pending_candidates_only_uses_active_connection_states() {
    assert!(is_active_for_pending(Some(&ConnectionState::Connecting)));
    assert!(is_active_for_pending(Some(&ConnectionState::Connected)));
    assert!(is_active_for_pending(Some(&ConnectionState::Reconnecting)));
    assert!(!is_active_for_pending(Some(&ConnectionState::Disconnected)));
    assert!(!is_active_for_pending(None));
}
