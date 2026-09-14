#[path = "../src/transport/webrtc/offer_guard.rs"]
mod offer_guard;

use mistlib_core::types::ConnectionState;
use offer_guard::{
    active_connection_count, create_failure_rollback, offer_action_for_snapshot, OfferAction,
    OfferCreateFailureRollback, SignalingSnapshot,
};

#[test]
fn existing_peer_action_depends_on_signaling_state() {
    // WASM is the polite peer: a colliding local offer yields, Stable applies
    // renegotiation in place, and transient states defer without teardown.
    let cases = [
        (
            "local offer without connection state",
            None,
            0,
            SignalingSnapshot::HaveLocalOffer,
            OfferAction::YieldAndApply,
        ),
        (
            "local offer while connecting",
            Some(ConnectionState::Connecting),
            1,
            SignalingSnapshot::HaveLocalOffer,
            OfferAction::YieldAndApply,
        ),
        (
            "stable",
            Some(ConnectionState::Connected),
            1,
            SignalingSnapshot::Stable,
            OfferAction::ApplyInPlace,
        ),
        (
            "other transient state",
            Some(ConnectionState::Connecting),
            1,
            SignalingSnapshot::Other,
            OfferAction::DeferTransient,
        ),
    ];

    for (name, state, active_count, signaling, expected) in cases {
        assert_eq!(
            offer_action_for_snapshot(true, false, state, active_count, 30, signaling),
            expected,
            "case: {name}"
        );
    }
}

#[test]
fn restarted_remote_always_replaces_its_stale_existing_peer() {
    // Restart information outranks every local signaling snapshot; applying,
    // yielding, or deferring against the dead peer would stall reconnection.
    for signaling in [
        SignalingSnapshot::Stable,
        SignalingSnapshot::HaveLocalOffer,
        SignalingSnapshot::Other,
    ] {
        assert_eq!(
            offer_action_for_snapshot(
                true,
                true,
                Some(ConnectionState::Connected),
                1,
                30,
                signaling,
            ),
            OfferAction::ReplacePeer,
            "signaling: {signaling:?}"
        );
    }
}

#[test]
fn new_peer_respects_capacity_regardless_of_restart_flag() {
    for remote_restarted in [false, true] {
        assert_eq!(
            offer_action_for_snapshot(
                false,
                remote_restarted,
                None,
                1,
                2,
                SignalingSnapshot::Other,
            ),
            OfferAction::Accept {
                newly_reserved: true
            },
            "under capacity; remote_restarted={remote_restarted}"
        );
        assert_eq!(
            offer_action_for_snapshot(
                false,
                remote_restarted,
                None,
                2,
                2,
                SignalingSnapshot::Other,
            ),
            OfferAction::IgnoreAtCapacity,
            "at capacity; remote_restarted={remote_restarted}"
        );
    }
}

#[test]
fn create_pc_failure_rolls_back_only_new_reservations() {
    assert_eq!(
        create_failure_rollback(true),
        OfferCreateFailureRollback::RemoveReservation
    );
    assert_eq!(
        create_failure_rollback(false),
        OfferCreateFailureRollback::KeepExistingState
    );
}

#[test]
fn active_count_includes_reconnecting_but_not_disconnected_or_failed() {
    let count = active_connection_count([
        ConnectionState::Connected,
        ConnectionState::Connecting,
        ConnectionState::Reconnecting,
        ConnectionState::Disconnected,
        ConnectionState::Failed,
    ]);

    assert_eq!(count, 3);
}
