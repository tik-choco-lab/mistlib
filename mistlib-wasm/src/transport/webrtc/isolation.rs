use mistlib_core::types::ConnectionState;

/// Pure predicate behind `schedule_isolation_recovery`'s decision to rotate
/// our own signaling identity (`signaler.reset_session()`). Kept
/// dependency-free (no `wasm_bindgen`/`web_sys`/`Arc<RwLock<..>>`) so it's
/// host-testable via the `#[path]` trick, same as `offer_guard`/
/// `request_guard` -- see `mistlib-wasm/tests/isolation.rs`.
///
/// True only when every tracked connection is non-connected (`states.all()`
/// over `Disconnected`/`Failed`/anything not
/// `Connected`/`Connecting`/`Reconnecting`) AND there is no in-flight
/// connection attempt (`in_flight_attempts == 0`).
///
/// The in-flight-attempts check exists because `connection_states` alone is
/// misleading right after a teardown: `cleanup_peer_connection` sets a peer's
/// state to `Disconnected` synchronously, but in a small (e.g. two-node) room
/// that can make *every* tracked peer read as non-connected for the whole
/// window between that teardown and the reconnect it queued up completing --
/// even though a reconnect is actively being negotiated with the very peer
/// this would rotate our identity away from. Treating that window as
/// "isolated" and resetting the session there is precisely wrong: the remote
/// sees the identity rotation as another restart and answers with its own
/// `Rejoin`, tearing down the in-flight reconnect and re-arming this same
/// check on its side -- a self-sustaining livelock (see `IsolationRecovery`'s
/// doc in the parent module for the full mechanism). `connection_attempt_ids`
/// is non-empty for exactly that window (it's populated by
/// `reserve_connection_attempt`/`connect()` and only cleared on a genuine
/// give-up), so requiring it to be empty closes the hole from the read side,
/// complementing `IsolationRecovery::Skip` closing it from the write side.
pub fn is_isolated(
    states: impl IntoIterator<Item = ConnectionState>,
    in_flight_attempts: usize,
) -> bool {
    if in_flight_attempts > 0 {
        return false;
    }
    // `.all()` over an empty iterator is vacuously `true` -- preserved
    // unchanged from the pre-Fix-3 behavior (a room with no tracked peers at
    // all still counts as isolated, e.g. right after the very first
    // `cleanup_peer_connection` with nothing else ever having connected).
    states.into_iter().all(|state| {
        !matches!(
            state,
            ConnectionState::Connected
                | ConnectionState::Connecting
                | ConnectionState::Reconnecting
        )
    })
}
