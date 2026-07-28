use bytes::Bytes;
use mistlib_core::types::{ConnectionState, DeliveryMethod};
use std::collections::VecDeque;

/// Cap on how many `ReliableOrdered` sends `WasmWebRtcTransport::send` will
/// defer for a single node while its peer exists but isn't `Connected`
/// yet/still (see `should_queue_reliable_send` below). Past this, the oldest
/// queued message is dropped to make room -- a peer stuck mid ICE-restart
/// grace for a long time (or one that never recovers and is eventually torn
/// down) shouldn't be allowed to accumulate unbounded memory.
pub const MAX_QUEUED_MESSAGES: usize = 64;
/// Byte-size companion to `MAX_QUEUED_MESSAGES`: a handful of large messages
/// can blow the memory budget well before the message count does.
pub const MAX_QUEUED_BYTES: usize = 256 * 1024;

/// Whether `WasmWebRtcTransport::send` should defer `data` for later
/// delivery (via `Peer::flush_send_queue`) instead of failing it outright
/// with "Not connected" -- decided purely from the delivery method and a
/// peer/state snapshot, so it's host-testable via the `#[path]` trick, same
/// as the rest of this directory's pure modules.
///
/// Only `ReliableOrdered` qualifies: `UnreliableOrdered`/`Unreliable` are
/// loss-tolerant by contract (that's the whole point of calling them
/// "unreliable"), so they keep the existing fail-fast semantics instead of
/// queuing behind a recovery that might never come.
///
/// Only queues while the peer still exists (there's an actual `Peer`/
/// `RTCPeerConnection` to eventually flush onto) and its connection state is
/// `Connecting` (a fresh connection whose DataChannel hasn't opened yet, or
/// -- since `recovery::state_after_ice_recovery` -- a peer mid ICE-restart
/// grace whose DataChannel isn't open) or `Reconnecting` (ICE `Disconnected`
/// grace, see `mark_suspect_disconnected`/the ICE `Disconnected` arm in
/// `Peer::setup_handlers`). A peer that doesn't exist, or one whose state is
/// `Disconnected`/`Failed`, has nothing left to recover into -- keep failing
/// those fast, exactly as before.
pub fn should_queue_reliable_send(
    method: DeliveryMethod,
    peer_exists: bool,
    state: ConnectionState,
) -> bool {
    peer_exists
        && method == DeliveryMethod::ReliableOrdered
        && matches!(
            state,
            ConnectionState::Connecting | ConnectionState::Reconnecting
        )
}

/// A bounded per-peer FIFO of deferred `ReliableOrdered` sends. Lives on
/// `Peer` (see its `send_queue` field) so it's torn down for free whenever
/// the peer itself is, rather than as a separate node-keyed map the
/// transport would have to keep in sync with every peer-removal path by
/// hand.
#[derive(Default)]
pub struct SendQueue {
    messages: VecDeque<Bytes>,
    total_bytes: usize,
}

impl SendQueue {
    /// Enqueues `data`, dropping the oldest queued message(s) first if doing
    /// so would exceed `MAX_QUEUED_MESSAGES` or `MAX_QUEUED_BYTES`. Returns
    /// `true` if anything was dropped, so the caller can `warn!` -- mirrors
    /// `PendingCandidates::push`'s `dropped_oldest` bool.
    pub fn push(&mut self, data: Bytes) -> bool {
        self.total_bytes += data.len();
        self.messages.push_back(data);

        let mut dropped_oldest = false;
        while self.messages.len() > MAX_QUEUED_MESSAGES || self.total_bytes > MAX_QUEUED_BYTES {
            match self.messages.pop_front() {
                Some(oldest) => {
                    self.total_bytes = self.total_bytes.saturating_sub(oldest.len());
                    dropped_oldest = true;
                }
                None => break,
            }
        }
        dropped_oldest
    }

    /// Removes and returns every queued message, in FIFO (oldest-first)
    /// order, for `Peer::flush_send_queue` to replay onto the now-open
    /// channel. Leaves the queue empty either way (including when nothing
    /// was queued).
    pub fn drain(&mut self) -> Vec<Bytes> {
        self.total_bytes = 0;
        self.messages.drain(..).collect()
    }

    /// Drops everything queued without replaying it, returning how many
    /// messages were dropped so the caller can `warn!` if it wasn't already
    /// empty. Used on final peer teardown (see `Peer::clear_send_queue`) --
    /// once a peer is gone for good, its deferred sends have nowhere left to
    /// go.
    pub fn clear(&mut self) -> usize {
        let dropped = self.messages.len();
        self.messages.clear();
        self.total_bytes = 0;
        dropped
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.messages.len()
    }
}
