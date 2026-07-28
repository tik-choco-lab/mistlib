use mistlib_core::types::ConnectionState;

/// Mirrors the two `web_sys::RtcIceConnectionState` variants that reach
/// `state_after_ice_recovery` (its only caller, `Peer::setup_handlers`'s
/// `oniceconnectionstatechange` handler, matches on
/// `Connected | Completed` before calling in), kept dependency-free (no
/// `web_sys`) so this stays host-testable via the `#[path]` trick, same as
/// `ice_restart` / `offer_guard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceRecoveryTrigger {
    Connected,
    Completed,
}

/// Decides the `ConnectionState` a peer should be left in when ICE reports
/// `Connected`/`Completed`. This replaces the old unconditional
/// `states.insert(remote_id, ConnectionState::Connecting)` that caused the
/// confirmed "sends fail forever after ICE recovers" bug:
///
/// `ConnectionState::Connected` is otherwise ONLY ever set by the DataChannel
/// `onopen` handler (`Peer::setup_dc_handlers`) or by
/// `WasmWebRtcTransport::cancel_suspect_grace` (the liveness-suspect path).
/// After an ICE restart, the existing (still-open) DataChannels never re-fire
/// `onopen` -- they were never closed in the first place -- so a peer that
/// ICE unconditionally demoted to `Connecting` on every `Connected`/
/// `Completed` transition stayed stuck there forever. Every subsequent
/// `send()` then failed with "Not connected" even though the DataChannels
/// were open and perfectly usable, until the DataChannel eventually closed
/// on its own and tore the whole session down (the field report this fixes:
/// 7+3 failed sends around an ICE-restart recovery, ending in "DataChannel
/// closed -> immediate disconnect").
///
/// Deciding from actual DataChannel readiness instead fixes this without
/// touching the fresh-connect path at all:
///
/// - Fresh connection, no DataChannel open yet (`has_open_channel = false`):
///   stays `Connecting`. The DC `onopen` handler is what promotes it to
///   `Connected` (and fires `emit_peer_connected`) -- unchanged from today.
/// - ICE-restart recovery, at least one DataChannel already `Open`
///   (`has_open_channel = true`): the DataChannel(s) survived the restart
///   and won't re-fire `onopen`, so this is the only place left able to
///   repair the state -- `Connected`.
/// - Flicker (ICE bounces between `Connected` and `Completed` while a
///   DataChannel is already open, or while none is open yet): both
///   `IceRecoveryTrigger` variants collapse to the same result here, so
///   firing this repeatedly for the same underlying condition is idempotent.
///
/// Callers must NOT emit `emit_peer_connected` off the back of a `Connected`
/// result from *this* function: unlike a fresh connection, the peer never
/// emitted `disconnected` during the grace period in the first place (ICE
/// `Disconnected` only starts a grace/holds the peer -- it doesn't tear
/// anything down or notify the app), so the app already considers the peer
/// connected. Only the DC `onopen` path's own `prev != Connected` check
/// decides whether to emit; this function's result plays no part in that.
pub fn state_after_ice_recovery(
    _trigger: IceRecoveryTrigger,
    has_open_channel: bool,
) -> ConnectionState {
    if has_open_channel {
        ConnectionState::Connected
    } else {
        ConnectionState::Connecting
    }
}
