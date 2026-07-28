#[path = "../src/transport/webrtc/isolation.rs"]
mod isolation;

use isolation::is_isolated;
use mistlib_core::types::ConnectionState;

#[test]
fn all_disconnected_with_no_attempts_is_isolated() {
    assert!(is_isolated(
        [ConnectionState::Disconnected, ConnectionState::Failed],
        0
    ));
}

#[test]
fn all_disconnected_with_one_in_flight_attempt_is_not_isolated() {
    // This is the exact bug: right after `cleanup_peer_connection` every
    // tracked peer reads as non-connected, but a reconnect can already be
    // under way (`connection_attempt_ids` non-empty) -- treating this as
    // isolated and rotating our signaling identity is what fed the livelock.
    assert!(!is_isolated(
        [ConnectionState::Disconnected, ConnectionState::Failed],
        1
    ));
}

#[test]
fn one_connected_with_no_attempts_is_not_isolated() {
    assert!(!is_isolated(
        [ConnectionState::Connected, ConnectionState::Disconnected],
        0
    ));
}

#[test]
fn one_connected_with_in_flight_attempts_is_not_isolated() {
    assert!(!is_isolated([ConnectionState::Connected], 2));
}

#[test]
fn connecting_and_reconnecting_states_count_as_not_isolated() {
    assert!(!is_isolated([ConnectionState::Connecting], 0));
    assert!(!is_isolated([ConnectionState::Reconnecting], 0));
}

#[test]
fn empty_state_map_with_no_attempts_is_isolated() {
    // Preserves the pre-existing `.all()`-on-empty-iterator semantics
    // (vacuously true): a room with no tracked peers at all still counts as
    // isolated. This test exists to make any future change to that
    // semantics deliberate rather than accidental.
    let states: Vec<ConnectionState> = Vec::new();
    assert!(is_isolated(states, 0));
}

#[test]
fn empty_state_map_with_in_flight_attempt_is_not_isolated() {
    let states: Vec<ConnectionState> = Vec::new();
    assert!(!is_isolated(states, 1));
}
