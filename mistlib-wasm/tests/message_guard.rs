#[path = "../src/transport/webrtc/message_guard.rs"]
mod message_guard;

use message_guard::{check_message_size, SizeCheck};
use mistlib_core::error::MistError;

#[test]
fn under_limit_and_below_80_percent_is_ok() {
    // MistError doesn't derive PartialEq, so Result<SizeCheck, MistError> as
    // a whole can't go through assert_eq! -- unwrap first and compare the Ok
    // payload instead.
    let result = check_message_size(100, 1000).unwrap();

    assert_eq!(result, SizeCheck::Ok);
}

#[test]
fn exactly_at_80_percent_is_near_limit() {
    let result = check_message_size(800, 1000).unwrap();

    assert_eq!(result, SizeCheck::NearLimit);
}

#[test]
fn just_under_80_percent_is_ok() {
    let result = check_message_size(799, 1000).unwrap();

    assert_eq!(result, SizeCheck::Ok);
}

#[test]
fn exactly_at_limit_is_near_limit_not_an_error() {
    let result = check_message_size(1000, 1000).unwrap();

    assert_eq!(result, SizeCheck::NearLimit);
}

#[test]
fn over_limit_is_rejected_with_size_and_limit() {
    let result = check_message_size(1001, 1000);

    match result {
        Err(MistError::MessageTooLarge { size, limit }) => {
            assert_eq!(size, 1001);
            assert_eq!(limit, 1000);
        }
        other => panic!("expected MessageTooLarge, got {:?}", other),
    }
}

#[test]
fn zero_limit_rejects_any_nonzero_message() {
    let result = check_message_size(1, 0);

    assert!(matches!(result, Err(MistError::MessageTooLarge { .. })));
}

#[test]
fn zero_limit_allows_zero_length_message() {
    // size (0) > limit (0) is false, so this passes; the 80% check is
    // skipped entirely when limit == 0 to avoid dividing by it.
    let result = check_message_size(0, 0).unwrap();

    assert_eq!(result, SizeCheck::Ok);
}
