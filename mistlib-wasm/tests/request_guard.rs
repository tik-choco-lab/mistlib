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
fn connected_with_open_data_channel_is_ignored() {
    let mut snapshot = snapshot(Some(ConnectionState::Connected));
    snapshot.peer_exists = true;
    snapshot.has_open_data_channel = true;

    assert_eq!(request_action_for_snapshot(snapshot), RequestAction::Ignore);
}

#[test]
fn connected_without_peer_is_cleaned_before_connecting() {
    assert_eq!(
        request_action_for_snapshot(snapshot(Some(ConnectionState::Connected))),
        RequestAction::CleanupAndConnect
    );
}

#[test]
fn connecting_with_attempt_is_ignored() {
    let mut snapshot = snapshot(Some(ConnectionState::Connecting));
    snapshot.has_attempt = true;

    assert_eq!(request_action_for_snapshot(snapshot), RequestAction::Ignore);
}

#[test]
fn connecting_without_attempt_is_cleaned_before_connecting() {
    assert_eq!(
        request_action_for_snapshot(snapshot(Some(ConnectionState::Connecting))),
        RequestAction::CleanupAndConnect
    );
}

#[test]
fn reconnecting_uses_same_attempt_guard_as_connecting() {
    let mut snapshot = snapshot(Some(ConnectionState::Reconnecting));
    assert_eq!(
        request_action_for_snapshot(snapshot),
        RequestAction::CleanupAndConnect
    );

    snapshot.has_attempt = true;
    assert_eq!(request_action_for_snapshot(snapshot), RequestAction::Ignore);
}

#[test]
fn disconnected_or_missing_state_connects_without_cleanup() {
    assert_eq!(
        request_action_for_snapshot(snapshot(Some(ConnectionState::Disconnected))),
        RequestAction::Connect
    );
    assert_eq!(
        request_action_for_snapshot(snapshot(None)),
        RequestAction::Connect
    );
}

#[test]
fn failed_state_is_cleaned_before_connecting() {
    assert_eq!(
        request_action_for_snapshot(snapshot(Some(ConnectionState::Failed))),
        RequestAction::CleanupAndConnect
    );
}

#[test]
fn remote_restarted_overrides_connected_with_open_data_channel() {
    // This is the exact bug: previously a stale Connected peer with an Open
    // DataChannel caused the Request to be ignored for tens of seconds after
    // the remote actually reloaded. Once the signaling layer tells us the
    // remote restarted, that cached view is known-stale and must not
    // suppress the reconnect.
    let mut snapshot = snapshot(Some(ConnectionState::Connected));
    snapshot.peer_exists = true;
    snapshot.has_open_data_channel = true;
    snapshot.remote_restarted = true;

    assert_eq!(
        request_action_for_snapshot(snapshot),
        RequestAction::CleanupAndConnect
    );
}

#[test]
fn remote_restarted_overrides_connecting_with_attempt() {
    let mut snapshot = snapshot(Some(ConnectionState::Connecting));
    snapshot.has_attempt = true;
    snapshot.remote_restarted = true;

    assert_eq!(
        request_action_for_snapshot(snapshot),
        RequestAction::CleanupAndConnect
    );
}

#[test]
fn remote_restarted_with_no_state_still_cleans_up_and_connects() {
    let mut snapshot = snapshot(None);
    snapshot.remote_restarted = true;

    assert_eq!(
        request_action_for_snapshot(snapshot),
        RequestAction::CleanupAndConnect
    );
}
