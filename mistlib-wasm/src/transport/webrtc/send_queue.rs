use bytes::Bytes;
use mistlib_core::types::{ConnectionState, DeliveryMethod};
use std::collections::VecDeque;

/// Cap on `ReliableOrdered` sends deferred for a peer whose channel is not
/// open while Connecting/Reconnecting. Evict oldest messages past the cap so
/// a peer that never recovers cannot accumulate unbounded memory.
pub const MAX_QUEUED_MESSAGES: usize = 64;
/// Byte-size companion to `MAX_QUEUED_MESSAGES`: a handful of large messages
/// can blow the memory budget well before the message count does.
pub const MAX_QUEUED_BYTES: usize = 256 * 1024;

/// Channel readiness is authoritative, even during logical reconnect grace.
/// Unreliable methods never defer or flush the reliable queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendAction {
    Write,
    FlushThenWrite,
    Defer,
    Fail,
}

/// Pure snapshot decision; the caller holds the reliable gate while taking
/// this snapshot and executing it, including any deferred FIFO flush.
pub fn send_action(
    method: DeliveryMethod,
    peer_exists: bool,
    state: ConnectionState,
    channel_open: bool,
    has_queued: bool,
) -> SendAction {
    if !peer_exists {
        return SendAction::Fail;
    }
    if channel_open {
        return if method == DeliveryMethod::ReliableOrdered && has_queued {
            SendAction::FlushThenWrite
        } else {
            SendAction::Write
        };
    }
    if method == DeliveryMethod::ReliableOrdered
        && matches!(
            state,
            ConnectionState::Connecting | ConnectionState::Reconnecting
        )
    {
        SendAction::Defer
    } else {
        SendAction::Fail
    }
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

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Leave the head in place until its channel write succeeds. A failed
    /// flush must preserve the older frames and prevent new sends overtaking.
    pub fn front(&self) -> Option<&Bytes> {
        self.messages.front()
    }

    pub fn pop_front(&mut self) -> Option<Bytes> {
        let data = self.messages.pop_front()?;
        self.total_bytes -= data.len();
        Some(data)
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
