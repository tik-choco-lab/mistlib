#[path = "../src/transport/webrtc/message_guard.rs"]
mod message_guard;

use message_guard::{check_message_size, SizeCheck};
use mistlib_core::error::MistError;

#[test]
fn classifies_messages_at_size_boundaries() {
    let cases = [
        ("well below warning", 100, 1000, SizeCheck::Ok),
        ("just below warning", 799, 1000, SizeCheck::Ok),
        ("warning boundary", 800, 1000, SizeCheck::NearLimit),
        ("exactly at limit", 1000, 1000, SizeCheck::NearLimit),
        ("zero sized with zero limit", 0, 0, SizeCheck::Ok),
    ];

    for (name, size, limit, expected) in cases {
        assert_eq!(
            check_message_size(size, limit).unwrap(),
            expected,
            "case: {name}"
        );
    }
}

#[test]
fn rejects_oversized_messages_with_the_actual_size_and_limit() {
    for (name, size, limit) in [("ordinary limit", 1001, 1000), ("zero limit", 1, 0)] {
        match check_message_size(size, limit) {
            Err(MistError::MessageTooLarge {
                size: actual_size,
                limit: actual_limit,
            }) => {
                assert_eq!(actual_size, size, "size for case: {name}");
                assert_eq!(actual_limit, limit, "limit for case: {name}");
            }
            other => panic!("case {name}: expected MessageTooLarge, got {other:?}"),
        }
    }
}
