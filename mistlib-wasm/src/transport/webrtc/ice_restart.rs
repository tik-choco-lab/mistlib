/// Whether an ICE-restart offer should be fired for a peer whose ICE state
/// just went `Disconnected`. Kept as a pure/host-testable decision (see
/// `mistlib-wasm/tests/ice_restart.rs`) -- the wasm-only caller
/// (`Peer::setup_handlers`'s `oniceconnectionstatechange` handler) supplies
/// the three inputs gathered from the browser APIs and `disconnected_since`:
///
/// - `is_new_grace`: this is the transition that *started* the current
///   disconnected-grace period, not a repeat/flicker while one is already
///   running -- restart is attempted at most once per grace period.
/// - `is_initiator`: only the lower-NodeId side restarts, so both peers
///   don't race to send competing restart offers at once. The other side's
///   restart offer arrives as a normal `Offer` and is handled by the
///   in-place renegotiation path (`offer_guard::OfferAction::ApplyInPlace`).
/// - `signaling_is_stable`: an ICE restart is itself a renegotiation: firing
///   one while a prior offer/answer exchange is still in flight would
///   collide with it.
pub fn should_trigger_ice_restart(
    is_new_grace: bool,
    is_initiator: bool,
    signaling_is_stable: bool,
) -> bool {
    is_new_grace && is_initiator && signaling_is_stable
}

/// A transient consent-check failure must not restart a path that is still
/// receiving SCTP/DTLS traffic. An in-place restart resets the native peer's
/// candidate pairs, so it can turn a recoverable blip into a lost association.
pub const ICE_RESTART_DEBOUNCE_MS: u32 = 2_000;

pub fn restart_has_inbound_silence(last_rx_age: Option<std::time::Duration>) -> bool {
    last_rx_age.is_none_or(|age| {
        age >= std::time::Duration::from_millis(u64::from(ICE_RESTART_DEBOUNCE_MS))
    })
}
