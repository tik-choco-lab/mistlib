#[path = "../src/transport/webrtc/backpressure.rs"]
mod backpressure;

use backpressure::{backpressure_action, BackpressureAction};
use mistlib_core::types::DeliveryMethod;

const HIGH_WATERMARK: u32 = 1024 * 1024;

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
