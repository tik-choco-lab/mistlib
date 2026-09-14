#[path = "../src/transport/webrtc/isolation.rs"]
mod isolation;

use isolation::is_isolated;
use mistlib_core::types::ConnectionState;

#[test]
fn detects_isolation_from_peer_states_and_inflight_attempts() {
    // In-flight attempts are significant even when every tracked peer is
    // disconnected: rotating the signaling identity in that state caused a
    // reconnect livelock. An empty state map remains vacuously isolated only
    // when no attempt is running.
    let cases: &[(&str, &[ConnectionState], usize, bool)] = &[
        (
            "all disconnected",
            &[ConnectionState::Disconnected, ConnectionState::Failed],
            0,
            true,
        ),
        (
            "all disconnected with attempt",
            &[ConnectionState::Disconnected, ConnectionState::Failed],
            1,
            false,
        ),
        (
            "one connected",
            &[ConnectionState::Connected, ConnectionState::Disconnected],
            0,
            false,
        ),
        (
            "connected with attempts",
            &[ConnectionState::Connected],
            2,
            false,
        ),
        ("connecting", &[ConnectionState::Connecting], 0, false),
        ("reconnecting", &[ConnectionState::Reconnecting], 0, false),
        ("empty", &[], 0, true),
        ("empty with attempt", &[], 1, false),
    ];

    for (name, states, inflight_attempts, expected) in cases {
        assert_eq!(
            is_isolated(states.iter().copied(), *inflight_attempts),
            *expected,
            "case: {name}"
        );
    }
}
