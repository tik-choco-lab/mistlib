use mistlib_core::transport::track_audience::TrackAudience;
use mistlib_core::types::NodeId;

fn node(id: &str) -> NodeId {
    NodeId(id.to_string())
}

#[test]
fn null_or_undefined_audience_is_everyone_and_empty_is_nobody() {
    // `set_local_track_audience(id, null | undefined)` -> `None`; `[]` -> `Some([])`.
    assert!(TrackAudience::new(None).allows(&node("any")));
    assert!(!TrackAudience::new(Some(vec![])).allows(&node("any")));
}

#[test]
fn only_listed_peers_are_in_the_audience() {
    let audience = TrackAudience::new(Some(vec![node("a"), node("b")]));
    assert!(audience.allows(&node("a")));
    assert!(audience.allows(&node("b")));
    assert!(!audience.allows(&node("c")));
}
