use super::*;
use crate::transports::webrtc::exceeds_warn_threshold;
use bytes::Bytes;
use mistlib_core::error::MistError;
use mistlib_core::transport::Transport;
use mistlib_core::types::{DeliveryMethod, NodeId};

/// The size check is the very first thing `Transport::send` does, ahead of
/// the peer/DC lookup, so it must reject an oversized payload even when the
/// target node has no live connection at all -- proving `dc.send` is never
/// reached for a rejected payload (there is no `dc` to reach here).
#[tokio::test]
async fn send_rejects_payload_over_max_message_bytes() {
    let t = make_transport();
    t.set_max_message_bytes(10);

    let err = t
        .send(
            &NodeId("nonexistent".to_string()),
            Bytes::from(vec![0u8; 11]),
            DeliveryMethod::ReliableOrdered,
        )
        .await
        .expect_err("payload exceeding max_message_bytes must be rejected");

    match err {
        MistError::MessageTooLarge { size, limit } => {
            assert_eq!(size, 11);
            assert_eq!(limit, 10);
        }
        other => panic!("expected MessageTooLarge, got {other:?}"),
    }
}

/// A payload exactly at the limit must pass the size gate. There's no live
/// peer in this test, so `send` still errors downstream -- the assertion is
/// that the error is NOT `MessageTooLarge`, i.e. the size check let it through.
#[tokio::test]
async fn send_allows_payload_at_exact_max_message_bytes() {
    let t = make_transport();
    t.set_max_message_bytes(10);

    let err = t
        .send(
            &NodeId("nonexistent".to_string()),
            Bytes::from(vec![0u8; 10]),
            DeliveryMethod::ReliableOrdered,
        )
        .await
        .expect_err("no real peer exists in this test, so send fails downstream regardless");

    assert!(
        !matches!(err, MistError::MessageTooLarge { .. }),
        "a payload exactly at the limit must not be rejected by the size gate, got {err:?}"
    );
}

/// Sanity-checks the native-side default (before any config wiring runs)
/// against SPEC-13's documented default of 64KiB.
#[tokio::test]
async fn default_max_message_bytes_is_64kib() {
    let t = make_transport();

    let err = t
        .send(
            &NodeId("nonexistent".to_string()),
            Bytes::from(vec![0u8; 65536 + 1]),
            DeliveryMethod::ReliableOrdered,
        )
        .await
        .expect_err("must be rejected under the default 64KiB limit");

    match err {
        MistError::MessageTooLarge { size, limit } => {
            assert_eq!(size, 65536 + 1);
            assert_eq!(limit, 65536);
        }
        other => panic!("expected MessageTooLarge, got {other:?}"),
    }
}

/// Config-driven changes to the limit must take effect (SPEC-13 acceptance
/// criterion #3): lowering it below a previously-fine payload now rejects it.
#[tokio::test]
async fn set_max_message_bytes_changes_the_effective_limit() {
    let t = make_transport();
    let payload = Bytes::from(vec![0u8; 100]);

    let err_before = t
        .send(
            &NodeId("nonexistent".to_string()),
            payload.clone(),
            DeliveryMethod::ReliableOrdered,
        )
        .await
        .expect_err("no live peer, so this still errors, but not with MessageTooLarge");
    assert!(!matches!(err_before, MistError::MessageTooLarge { .. }));

    t.set_max_message_bytes(50);

    let err_after = t
        .send(
            &NodeId("nonexistent".to_string()),
            payload,
            DeliveryMethod::ReliableOrdered,
        )
        .await
        .expect_err("payload now exceeds the lowered limit");
    match err_after {
        MistError::MessageTooLarge { size, limit } => {
            assert_eq!(size, 100);
            assert_eq!(limit, 50);
        }
        other => panic!("expected MessageTooLarge, got {other:?}"),
    }
}

/// Pure-function coverage for the 80%-of-limit warn boundary (SPEC-13
/// SHOULD #6), independent of capturing `tracing` output.
#[test]
fn exceeds_warn_threshold_is_strictly_greater_than_80_percent() {
    assert!(
        !exceeds_warn_threshold(80, 100),
        "exactly 80% must not (yet) warn"
    );
    assert!(exceeds_warn_threshold(81, 100), "just over 80% must warn");
    assert!(
        !exceeds_warn_threshold(52428, 65536),
        "80% of the default 64KiB limit must not warn"
    );
    assert!(
        exceeds_warn_threshold(52429, 65536),
        "just over 80% of the default 64KiB limit must warn"
    );
}
