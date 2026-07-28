#[path = "../src/transport/webrtc/offer_guard.rs"]
mod offer_guard;

use mistlib_core::types::ConnectionState;
use offer_guard::{
    active_connection_count, create_failure_rollback, offer_action_for_snapshot, OfferAction,
    OfferCreateFailureRollback, SignalingSnapshot,
};

#[test]
fn existing_peer_with_have_local_offer_yields_and_applies() {
    // Perfect negotiation: wasm is unconditionally polite. An inbound offer
    // colliding with our own in-flight offer (HaveLocalOffer) always yields
    // -- there is no impolite/id-comparison branch left, unlike the old
    // glare-ignore behavior this replaces.
    let action =
        offer_action_for_snapshot(true, false, None, 0, 30, SignalingSnapshot::HaveLocalOffer);

    assert_eq!(action, OfferAction::YieldAndApply);
}

#[test]
fn existing_peer_with_have_local_offer_yields_regardless_of_connection_state() {
    // Same HaveLocalOffer collision, but with a live Connecting state on the
    // peer (as opposed to `None` above) -- still yields, since the decision
    // depends only on signaling state, not on connection bookkeeping.
    let action = offer_action_for_snapshot(
        true,
        false,
        Some(ConnectionState::Connecting),
        1,
        30,
        SignalingSnapshot::HaveLocalOffer,
    );

    assert_eq!(action, OfferAction::YieldAndApply);
}

#[test]
fn existing_peer_with_stable_signaling_applies_in_place() {
    // Stable means there's no collision to resolve -- it's a genuine
    // renegotiation and must be applied in-place.
    let action = offer_action_for_snapshot(
        true,
        false,
        Some(ConnectionState::Connected),
        1,
        30,
        SignalingSnapshot::Stable,
    );

    assert_eq!(action, OfferAction::ApplyInPlace);
}

#[test]
fn existing_peer_with_other_signaling_state_is_deferred_without_teardown() {
    // Neither Stable nor HaveLocalOffer (e.g. HaveRemoteOffer, Closed): not a
    // renegotiation we can apply in-place, and not our own offer colliding
    // with theirs either -- deferred, not torn down and recreated
    // (mistlib-native's equivalent never discards an existing peer over an
    // offer either, it just returns an Err and leaves the live connection
    // as-is).
    let action = offer_action_for_snapshot(
        true,
        false,
        Some(ConnectionState::Connecting),
        1,
        30,
        SignalingSnapshot::Other,
    );

    assert_eq!(action, OfferAction::DeferTransient);
}

#[test]
fn existing_peer_is_always_resolved_or_deferred_never_recreated() {
    // Exhaustive-ish sanity check standing in for the "existing peer never
    // reaches Accept" invariant: no signaling_state with peer_exists=true
    // should ever produce a from-scratch Accept -- only ApplyInPlace,
    // YieldAndApply, or DeferTransient.
    for signaling_state in [
        SignalingSnapshot::Stable,
        SignalingSnapshot::HaveLocalOffer,
        SignalingSnapshot::Other,
    ] {
        let action = offer_action_for_snapshot(
            true,
            false,
            Some(ConnectionState::Connecting),
            1,
            30,
            signaling_state,
        );
        assert!(
            !matches!(action, OfferAction::Accept { .. }),
            "peer_exists=true produced Accept for signaling_state={:?}: {:?}",
            signaling_state,
            action
        );
    }
}

#[test]
fn offer_at_capacity_is_ignored_without_reservation() {
    let action = offer_action_for_snapshot(false, false, None, 2, 2, SignalingSnapshot::Other);

    assert_eq!(action, OfferAction::IgnoreAtCapacity);
}

#[test]
fn offer_without_state_reserves_when_capacity_allows() {
    let action = offer_action_for_snapshot(false, false, None, 1, 2, SignalingSnapshot::Other);

    assert_eq!(
        action,
        OfferAction::Accept {
            newly_reserved: true
        }
    );
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
fn remote_restarted_replaces_peer_instead_of_applying_in_place_when_stable() {
    // This is the exact bug: an offer from a remote that restarted must never
    // be grafted onto the stale peer's Stable-but-dead RTCPeerConnection.
    let action = offer_action_for_snapshot(
        true,
        true,
        Some(ConnectionState::Connected),
        1,
        30,
        SignalingSnapshot::Stable,
    );

    assert_eq!(action, OfferAction::ReplacePeer);
}

#[test]
fn remote_restarted_replaces_peer_instead_of_yielding_when_have_local_offer() {
    // No real glare to resolve against a peer instance that no longer
    // exists -- ReplacePeer outranks YieldAndApply.
    let action = offer_action_for_snapshot(
        true,
        true,
        Some(ConnectionState::Connecting),
        1,
        30,
        SignalingSnapshot::HaveLocalOffer,
    );

    assert_eq!(action, OfferAction::ReplacePeer);
}

#[test]
fn remote_restarted_replaces_peer_instead_of_deferring_when_other() {
    // Deferring would mean waiting for a retry that hits the same dead peer
    // -- ReplacePeer outranks DeferTransient.
    let action = offer_action_for_snapshot(
        true,
        true,
        Some(ConnectionState::Connecting),
        1,
        30,
        SignalingSnapshot::Other,
    );

    assert_eq!(action, OfferAction::ReplacePeer);
}

#[test]
fn remote_restarted_without_existing_peer_is_a_normal_new_connection() {
    // No peer exists locally for this remote at all, so a "restart" signal
    // is moot -- this is just an ordinary new connection under capacity.
    let action = offer_action_for_snapshot(false, true, None, 1, 2, SignalingSnapshot::Other);

    assert_eq!(
        action,
        OfferAction::Accept {
            newly_reserved: true
        }
    );
}

#[test]
fn remote_restarted_without_existing_peer_still_respects_capacity() {
    // A restart signal must not be usable to bypass the connection cap.
    let action = offer_action_for_snapshot(false, true, None, 2, 2, SignalingSnapshot::Other);

    assert_eq!(action, OfferAction::IgnoreAtCapacity);
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
