#[path = "../../src/transport/webrtc/ice_restart.rs"]
mod ice_restart;

use ice_restart::should_trigger_ice_restart;
use std::time::Duration;

#[test]
fn triggers_only_for_a_new_initiator_grace_with_stable_signaling() {
    let cases = [
        ("all conditions met", true, true, true, true),
        ("repeat in same grace", false, true, true, false),
        ("non initiator", true, false, true, false),
        ("unstable signaling", true, true, false, false),
    ];

    for (name, is_new_grace, is_initiator, signaling_is_stable, expected) in cases {
        assert_eq!(
            should_trigger_ice_restart(is_new_grace, is_initiator, signaling_is_stable),
            expected,
            "case: {name}"
        );
    }
}

#[test]
fn live_sctp_path_defers_restart_but_silence_allows_repair() {
    use ice_restart::{restart_has_inbound_silence, ICE_RESTART_DEBOUNCE_MS};
    let window = u64::from(ICE_RESTART_DEBOUNCE_MS);
    // Transport-counter growth counts even when reliable reassembly is stalled.
    for age in [0, 50, window - 1] {
        assert!(!restart_has_inbound_silence(Some(Duration::from_millis(
            age
        ))));
    }
    assert!(restart_has_inbound_silence(Some(Duration::from_millis(
        window
    ))));
    assert!(restart_has_inbound_silence(Some(Duration::from_secs(30))));
    assert!(restart_has_inbound_silence(None));
}
