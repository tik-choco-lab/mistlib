#[path = "../src/transport/webrtc/recovery.rs"]
mod recovery;

use mistlib_core::types::ConnectionState;
use recovery::{state_after_ice_recovery, IceRecoveryTrigger};

#[test]
fn restart_recovery_with_open_channel_repairs_to_connected() {
    // The confirmed-bug case: ICE recovers after a restart and the existing
    // DataChannel is still open (it never closed, so it never re-fires
    // `onopen`) -- this must land on `Connected`, not stay `Connecting`.
    assert_eq!(
        state_after_ice_recovery(IceRecoveryTrigger::Connected, true),
        ConnectionState::Connected
    );
}

#[test]
fn fresh_connect_with_no_open_channel_stays_connecting() {
    // A brand-new connection: ICE reaches Connected before any DataChannel
    // has opened. The DC `onopen` handler is still responsible for the
    // eventual promotion to `Connected`.
    assert_eq!(
        state_after_ice_recovery(IceRecoveryTrigger::Connected, false),
        ConnectionState::Connecting
    );
}

#[test]
fn completed_trigger_behaves_the_same_as_connected() {
    assert_eq!(
        state_after_ice_recovery(IceRecoveryTrigger::Completed, true),
        ConnectionState::Connected
    );
    assert_eq!(
        state_after_ice_recovery(IceRecoveryTrigger::Completed, false),
        ConnectionState::Connecting
    );
}

#[test]
fn flicker_between_connected_and_completed_is_idempotent_once_channel_is_open() {
    let a = state_after_ice_recovery(IceRecoveryTrigger::Connected, true);
    let b = state_after_ice_recovery(IceRecoveryTrigger::Completed, true);
    let c = state_after_ice_recovery(IceRecoveryTrigger::Connected, true);
    assert_eq!(a, ConnectionState::Connected);
    assert_eq!(a, b);
    assert_eq!(b, c);
}

#[test]
fn flicker_while_channel_still_not_open_stays_connecting_every_time() {
    let a = state_after_ice_recovery(IceRecoveryTrigger::Connected, false);
    let b = state_after_ice_recovery(IceRecoveryTrigger::Completed, false);
    assert_eq!(a, ConnectionState::Connecting);
    assert_eq!(a, b);
}
