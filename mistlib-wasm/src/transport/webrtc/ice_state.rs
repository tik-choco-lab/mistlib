use super::recovery::{state_after_ice_recovery, IceRecoveryTrigger};
use mistlib_core::types::{ConnectionState, NodeId};
use std::collections::HashMap;
use std::sync::RwLock;
use web_sys::RtcIceConnectionState;

/// Shared by the browser ICE callback and host guard tests. Commit state first,
/// release the guard, then run cleanup/notifications (which can read this map
/// synchronously through the engine's network transport).
pub(super) fn handle_ice_state_change(
    states: &RwLock<HashMap<NodeId, ConnectionState>>,
    remote: &NodeId,
    ice: RtcIceConnectionState,
    has_open_channel: bool,
    after_update: impl FnOnce(Option<ConnectionState>),
) {
    let state = match ice {
        RtcIceConnectionState::Connected => Some(state_after_ice_recovery(
            IceRecoveryTrigger::Connected,
            has_open_channel,
        )),
        RtcIceConnectionState::Completed => Some(state_after_ice_recovery(
            IceRecoveryTrigger::Completed,
            has_open_channel,
        )),
        RtcIceConnectionState::Disconnected => Some(ConnectionState::Reconnecting),
        RtcIceConnectionState::Failed | RtcIceConnectionState::Closed => {
            Some(ConnectionState::Disconnected)
        }
        RtcIceConnectionState::Checking => Some(ConnectionState::Connecting),
        _ => None,
    };
    if let Some(state) = state {
        states
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(remote.clone(), state);
    }
    after_update(state);
}
