#[path = "../src/transport/webrtc/backpressure.rs"]
mod backpressure;

use backpressure::{backpressure_action, BackpressureAction};
use mistlib_core::types::DeliveryMethod;

const HIGH_WATERMARK: u32 = 1024 * 1024;

#[test]
fn below_watermark_sends_now_regardless_of_method() {
    for method in [
        DeliveryMethod::ReliableOrdered,
        DeliveryMethod::UnreliableOrdered,
        DeliveryMethod::Unreliable,
    ] {
        assert_eq!(
            backpressure_action(HIGH_WATERMARK - 1, HIGH_WATERMARK, method),
            BackpressureAction::SendNow
        );
    }
}

#[test]
fn exactly_at_watermark_sends_now() {
    assert_eq!(
        backpressure_action(
            HIGH_WATERMARK,
            HIGH_WATERMARK,
            DeliveryMethod::ReliableOrdered
        ),
        BackpressureAction::SendNow
    );
}

#[test]
fn over_watermark_reliable_waits_then_sends() {
    assert_eq!(
        backpressure_action(
            HIGH_WATERMARK + 1,
            HIGH_WATERMARK,
            DeliveryMethod::ReliableOrdered
        ),
        BackpressureAction::WaitThenSend
    );
}

#[test]
fn over_watermark_unreliable_ordered_drops() {
    assert_eq!(
        backpressure_action(
            HIGH_WATERMARK + 1,
            HIGH_WATERMARK,
            DeliveryMethod::UnreliableOrdered
        ),
        BackpressureAction::Drop
    );
}

#[test]
fn over_watermark_unreliable_drops() {
    assert_eq!(
        backpressure_action(
            HIGH_WATERMARK + 1,
            HIGH_WATERMARK,
            DeliveryMethod::Unreliable
        ),
        BackpressureAction::Drop
    );
}
