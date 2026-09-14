#[path = "../src/transport/webrtc/offer_failure.rs"]
#[allow(dead_code)]
mod offer_failure;

use offer_failure::should_rebuild_after_offer_failure;

#[test]
fn transport_role_failure_requires_unsuccessful_rollback() {
    let error = "OperationError: Failed to apply the description for m= section with mid='0': Failed to set SSL role for the transport.";
    assert!(should_rebuild_after_offer_failure(error, false));
    assert!(!should_rebuild_after_offer_failure(error, true));
}

#[test]
fn other_failures_never_request_replacement() {
    for error in [
        "",
        "InvalidStateError",
        "Invalid SDP",
        "OperationError: network failure",
    ] {
        for stable in [false, true] {
            assert!(!should_rebuild_after_offer_failure(error, stable));
        }
    }
}
