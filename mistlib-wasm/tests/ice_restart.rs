#[path = "../src/transport/webrtc/ice_restart.rs"]
mod ice_restart;

use ice_restart::should_trigger_ice_restart;

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
