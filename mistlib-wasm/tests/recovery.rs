#[path = "../src/transport/webrtc/recovery.rs"]
mod recovery;

use mistlib_core::types::ConnectionState;
use recovery::{state_after_ice_recovery, IceRecoveryTrigger};

#[test]
fn derives_connection_state_from_data_channel_readiness() {
    // During ICE restart recovery, an already-open DataChannel will not fire
    // onopen again, so both ICE success triggers must restore Connected. A
    // fresh connection remains Connecting until its channel opens.
    let cases = [
        (
            "connected with open channel",
            IceRecoveryTrigger::Connected,
            true,
            ConnectionState::Connected,
        ),
        (
            "connected without open channel",
            IceRecoveryTrigger::Connected,
            false,
            ConnectionState::Connecting,
        ),
        (
            "completed with open channel",
            IceRecoveryTrigger::Completed,
            true,
            ConnectionState::Connected,
        ),
        (
            "completed without open channel",
            IceRecoveryTrigger::Completed,
            false,
            ConnectionState::Connecting,
        ),
    ];

    for (name, trigger, has_open_channel, expected) in cases {
        assert_eq!(
            state_after_ice_recovery(trigger, has_open_channel),
            expected,
            "case: {name}"
        );
    }
}
