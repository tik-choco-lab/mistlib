#[path = "../../src/transport/webrtc/backpressure.rs"]
mod backpressure;

use backpressure::{
    backpressure_action, drain_timeout_action, BackpressureAction, DrainTimeoutAction,
};
use mistlib_core::types::DeliveryMethod;

const HIGH_WATERMARK: u32 = 1024 * 1024;
const HARD_LIMIT: u32 = 8 * 1024 * 1024;

#[test]
fn chooses_action_at_and_around_the_watermark() {
    let cases = [
        (
            "reliable below",
            HIGH_WATERMARK - 1,
            DeliveryMethod::ReliableOrdered,
            BackpressureAction::SendNow,
        ),
        (
            "unreliable ordered below",
            HIGH_WATERMARK - 1,
            DeliveryMethod::UnreliableOrdered,
            BackpressureAction::SendNow,
        ),
        (
            "unreliable below",
            HIGH_WATERMARK - 1,
            DeliveryMethod::Unreliable,
            BackpressureAction::SendNow,
        ),
        (
            "exactly at watermark",
            HIGH_WATERMARK,
            DeliveryMethod::ReliableOrdered,
            BackpressureAction::SendNow,
        ),
        (
            "reliable above",
            HIGH_WATERMARK + 1,
            DeliveryMethod::ReliableOrdered,
            BackpressureAction::WaitThenSend,
        ),
        (
            "unreliable ordered above",
            HIGH_WATERMARK + 1,
            DeliveryMethod::UnreliableOrdered,
            BackpressureAction::Drop,
        ),
        (
            "unreliable above",
            HIGH_WATERMARK + 1,
            DeliveryMethod::Unreliable,
            BackpressureAction::Drop,
        ),
    ];

    for (name, buffered_amount, method, expected) in cases {
        assert_eq!(
            backpressure_action(buffered_amount, HIGH_WATERMARK, method),
            expected,
            "case: {name}"
        );
    }
}

#[test]
fn timer_expiry_requires_capacity_even_when_under_the_hard_limit() {
    let cases = [
        (
            "below high watermark",
            486_928,
            212,
            DrainTimeoutAction::SendNow,
        ),
        (
            "stale Firefox buffered amount",
            1_048_783,
            90,
            DrainTimeoutAction::KeepWaiting,
        ),
        (
            "exactly at hard limit",
            HARD_LIMIT - 212,
            212,
            DrainTimeoutAction::KeepWaiting,
        ),
        (
            "one byte over hard limit",
            HARD_LIMIT - 212,
            213,
            DrainTimeoutAction::KeepWaiting,
        ),
        (
            "buffer already over hard limit",
            HARD_LIMIT + 1,
            0,
            DrainTimeoutAction::KeepWaiting,
        ),
        #[cfg(target_pointer_width = "64")]
        (
            "message larger than u32 max",
            0,
            u32::MAX as usize + 1,
            DrainTimeoutAction::RejectOversized,
        ),
        (
            "maximum inputs",
            u32::MAX,
            usize::MAX,
            DrainTimeoutAction::KeepWaiting,
        ),
    ];

    for (name, buffered_amount, message_len, expected) in cases {
        assert_eq!(
            drain_timeout_action(buffered_amount, message_len, HIGH_WATERMARK, HARD_LIMIT),
            expected,
            "case: {name}"
        );
    }
}

#[test]
fn repeated_timer_wakes_wait_until_the_high_watermark_is_reached() {
    for _ in 0..10 {
        assert_eq!(
            drain_timeout_action(HIGH_WATERMARK + 1, 16 * 1024, HIGH_WATERMARK, HARD_LIMIT),
            DrainTimeoutAction::KeepWaiting
        );
    }
    assert_eq!(
        drain_timeout_action(HIGH_WATERMARK, 16 * 1024, HIGH_WATERMARK, HARD_LIMIT),
        DrainTimeoutAction::SendNow
    );
    assert_eq!(
        drain_timeout_action(0, HARD_LIMIT as usize + 1, HIGH_WATERMARK, HARD_LIMIT),
        DrainTimeoutAction::RejectOversized
    );
}
