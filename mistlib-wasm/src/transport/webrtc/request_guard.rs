use mistlib_core::types::ConnectionState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestAction {
    Ignore,
    CleanupAndConnect,
    Connect,
}

#[derive(Debug, Clone, Copy)]
pub struct RequestState {
    pub state: Option<ConnectionState>,
    pub peer_exists: bool,
    pub has_open_data_channel: bool,
    pub has_attempt: bool,
    /// True when the signaling layer has determined that this Request comes
    /// from a *newer session* of the peer than the one our existing peer
    /// connection belongs to -- i.e. the remote restarted (browser reload,
    /// process restart) without ever cleanly closing.
    pub remote_restarted: bool,
}

pub fn request_action_for_snapshot(snapshot: RequestState) -> RequestAction {
    // A restarted remote outranks every other signal below. `Connected` +
    // `has_open_data_channel` (or `Connecting` + `has_attempt`) normally mean
    // "an active session already exists, ignore this Request" -- but that
    // read is built entirely from *our* local bookkeeping about the *old*
    // peer connection, and a WebRTC page reload sends no clean close: the old
    // `RTCPeerConnection` keeps reporting itself Connected with an Open
    // DataChannel for tens of seconds (ICE consent freshness + grace period +
    // sweeper) after the remote instance that owned it is already gone. If we
    // trusted that stale view here, we'd ignore the *new* instance's
    // reconnect attempt for that whole window -- which is exactly the bug
    // this field exists to fix. Once the signaling layer tells us the remote
    // restarted, the cached state is known-stale, so always clean up and
    // connect to the new instance instead of consulting it.
    if snapshot.remote_restarted {
        return RequestAction::CleanupAndConnect;
    }

    match snapshot.state {
        Some(ConnectionState::Connected) => {
            if snapshot.peer_exists && snapshot.has_open_data_channel {
                RequestAction::Ignore
            } else {
                RequestAction::CleanupAndConnect
            }
        }
        Some(ConnectionState::Connecting | ConnectionState::Reconnecting) => {
            if snapshot.has_attempt {
                RequestAction::Ignore
            } else {
                RequestAction::CleanupAndConnect
            }
        }
        Some(ConnectionState::Failed) => RequestAction::CleanupAndConnect,
        Some(ConnectionState::Disconnected) | None => RequestAction::Connect,
    }
}
