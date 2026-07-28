use mistlib_core::types::DeliveryMethod;

/// `RTCDataChannel::send()` is fire-and-forget: it never surfaces
/// backpressure from a congested outbound queue on its own, so a slow or
/// lossy link lets `bufferedAmount` grow unbounded until the tab's memory
/// blows up. What to do about a send that would push a channel over the high
/// watermark depends only on the delivery method -- decided here as a pure
/// function so it's host-testable via the `#[path]` trick (see
/// `mistlib-wasm/tests/backpressure.rs`); the wasm-only caller
/// (`WasmWebRtcTransport::send`) does the actual `bufferedAmount()` read and,
/// for `WaitThenSend`, the `onbufferedamountlow` future-ization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureAction {
    /// Below the high watermark: send immediately.
    SendNow,
    /// Reliable: wait for `onbufferedamountlow` (bounded by a timeout)
    /// before sending, rather than piling more bytes onto an already
    /// congested channel.
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
        DeliveryMethod::ReliableOrdered => BackpressureAction::WaitThenSend,
        DeliveryMethod::UnreliableOrdered | DeliveryMethod::Unreliable => BackpressureAction::Drop,
    }
}
