use mistlib_core::types::DeliveryMethod;

/// `RTCDataChannel::send()` is fire-and-forget: it never surfaces
/// backpressure from a congested outbound queue on its own, so a slow or
/// lossy link lets `bufferedAmount` grow unbounded until the tab's memory
/// blows up. What to do about a send that would push a channel over the high
/// watermark depends only on the delivery method -- decided here as a pure
/// function so it's host-testable via the `#[path]` trick (see
/// `mistlib-wasm/tests/guards/backpressure.rs`); the wasm-only caller
/// (`WasmWebRtcTransport::send`) does the actual `bufferedAmount()` read and,
/// for `WaitThenSend`, the `onbufferedamountlow` future-ization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureAction {
    /// Below the high watermark: send immediately.
    SendNow,
    /// Reliable: wait for `onbufferedamountlow`. A timer periodically rechecks
    /// capacity when the browser misses that event; it never permits a refill.
    WaitThenSend,
    /// Unreliable(Ordered): freshness beats completeness -- drop rather than
    /// queue behind congestion.
    Drop,
}

pub fn backpressure_action(
    buffered_amount: u32,
    high_watermark: u32,
    method: DeliveryMethod,
) -> BackpressureAction {
    if buffered_amount <= high_watermark {
        return BackpressureAction::SendNow;
    }

    match method {
        DeliveryMethod::ReliableOrdered | DeliveryMethod::ReliableUnordered => {
            BackpressureAction::WaitThenSend
        }
        DeliveryMethod::UnreliableOrdered | DeliveryMethod::Unreliable => BackpressureAction::Drop,
    }
}

/// Whether an open reliable channel can accept a message after any drain wake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainTimeoutAction {
    SendNow,
    KeepWaiting,
    RejectOversized,
}

/// Never refill above the high watermark, even after repeated timer expiries.
/// The hard limit is only a defensive bound once normal capacity is available.
pub fn drain_timeout_action(
    buffered_amount: u32,
    message_len: usize,
    high_watermark: u32,
    hard_limit: u32,
) -> DrainTimeoutAction {
    if buffered_amount > high_watermark {
        DrainTimeoutAction::KeepWaiting
    } else if u64::from(buffered_amount).saturating_add(message_len as u64) <= u64::from(hard_limit)
    {
        DrainTimeoutAction::SendNow
    } else {
        DrainTimeoutAction::RejectOversized
    }
}
