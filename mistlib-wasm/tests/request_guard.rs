#[path = "../src/transport/webrtc/request_guard.rs"]
mod request_guard;

use mistlib_core::types::ConnectionState;
use request_guard::{request_action_for_snapshot, RequestAction, RequestState};

fn snapshot(state: Option<ConnectionState>) -> RequestState {
    RequestState {
        state,
        peer_exists: false,
        has_open_data_channel: false,
        has_attempt: false,
        remote_restarted: false,
    }
}

#[test]
fn chooses_action_from_the_current_connection_snapshot() {
    let mut connected_and_open = snapshot(Some(ConnectionState::Connected));
    connected_and_open.peer_exists = true;
    connected_and_open.has_open_data_channel = true;

    let mut connecting_with_attempt = snapshot(Some(ConnectionState::Connecting));
    connecting_with_attempt.has_attempt = true;

    let mut reconnecting_with_attempt = snapshot(Some(ConnectionState::Reconnecting));
    reconnecting_with_attempt.has_attempt = true;

    let cases = [
        (
            "connected with open channel",
            connected_and_open,
            RequestAction::Ignore,
        ),
        (
            "connected without peer",
            snapshot(Some(ConnectionState::Connected)),
            RequestAction::CleanupAndConnect,
        ),
        (
            "connecting with attempt",
            connecting_with_attempt,
            RequestAction::Ignore,
        ),
        (
            "connecting without attempt",
            snapshot(Some(ConnectionState::Connecting)),
            RequestAction::CleanupAndConnect,
        ),
        (
            "reconnecting with attempt",
            reconnecting_with_attempt,
            RequestAction::Ignore,
        ),
        (
            "reconnecting without attempt",
            snapshot(Some(ConnectionState::Reconnecting)),
            RequestAction::CleanupAndConnect,
        ),
        (
            "failed",
            snapshot(Some(ConnectionState::Failed)),
            RequestAction::CleanupAndConnect,
        ),
        (
            "disconnected",
            snapshot(Some(ConnectionState::Disconnected)),
            RequestAction::Connect,
        ),
        ("missing state", snapshot(None), RequestAction::Connect),
    ];

    for (name, snapshot, expected) in cases {
        assert_eq!(
            request_action_for_snapshot(snapshot),
            expected,
            "case: {name}"
        );
    }
}

#[test]
fn remote_restart_overrides_all_stale_local_snapshots() {
    // A stale Connected peer can keep an Open DataChannel for tens of seconds
    // after the remote reloads. Once signaling reports that restart, every
    // local snapshot must clean up and reconnect.
    let mut connected_and_open = snapshot(Some(ConnectionState::Connected));
    connected_and_open.peer_exists = true;
    connected_and_open.has_open_data_channel = true;

    let mut connecting_with_attempt = snapshot(Some(ConnectionState::Connecting));
    connecting_with_attempt.has_attempt = true;

    for (name, mut snapshot) in [
        ("connected with open channel", connected_and_open),
        ("connecting with attempt", connecting_with_attempt),
        ("missing state", snapshot(None)),
    ] {
        snapshot.remote_restarted = true;
        assert_eq!(
            request_action_for_snapshot(snapshot),
            RequestAction::CleanupAndConnect,
            "case: {name}"
        );
    }
}
