#[path = "../src/transport/webrtc/ice_restart.rs"]
mod ice_restart;

use ice_restart::should_trigger_ice_restart;

#[test]
fn new_grace_as_initiator_with_stable_signaling_triggers_restart() {
    assert!(should_trigger_ice_restart(true, true, true));
}

#[test]
fn repeat_within_same_grace_does_not_retrigger() {
    assert!(!should_trigger_ice_restart(false, true, true));
}

#[test]
fn non_initiator_does_not_trigger_even_on_new_grace() {
    assert!(!should_trigger_ice_restart(true, false, true));
}

#[test]
fn unstable_signaling_skips_restart() {
    assert!(!should_trigger_ice_restart(true, true, false));
}

#[test]
fn all_conditions_false_does_not_trigger() {
    assert!(!should_trigger_ice_restart(false, false, false));
}
