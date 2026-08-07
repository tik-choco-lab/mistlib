use super::message::MessageContent;
use crate::error::Result;
use crate::types::{HostSendSync, NodeId, SessionReestablishedHook};
use async_trait::async_trait;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait Signaler: HostSendSync {
    async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> Result<()>;
    async fn reset_session(&self) -> Result<()> {
        Ok(())
    }

    /// Registers a hook called when the signaling session is reestablished.
    /// Implementations that do not support reconnect may leave this as a no-op.
    fn set_on_session_reestablished(&self, _hook: SessionReestablishedHook) {}

    /// Notifies this signaler that live traffic just arrived from `peer` --
    /// over *any* transport, not necessarily this signaler's own.
    ///
    /// **A signaler that keeps expiring per-peer state must override this.**
    /// The Nostr signaler's [`DiscoveryTable`] binds a node id to a signaling
    /// pubkey with an expiry that is otherwise only pushed forward by
    /// discovery re-announcements and by inbound relay messages. Once a pair
    /// is connected, `RoutedSignaler` prefers the overlay, so no relay
    /// messages flow between them and the binding lapses even though the peer
    /// is plainly alive. The next time signaling has to fall back to the
    /// relay (an ICE restart, a reconnect after a blip) the lapsed side
    /// rejects the message in
    /// [`accept_sender_for_payload`](crate::signaling::nostr::accept_sender_for_payload),
    /// and it fails *one-directionally and silently* -- the peer that sent
    /// the original `Request` still accepts, because that is remembered for
    /// the whole session, while the peer that only ever had the binding does
    /// not.
    ///
    /// Signalers with no expiring per-peer state (overlay, sim, WebSocket)
    /// correctly keep the default no-op.
    ///
    /// ## Cost, if this ever shows up in a profile
    ///
    /// This runs on every inbound data message, and the Nostr implementation's
    /// [`DiscoveryTable::touch_node`] sweeps the table first, so the work is
    /// O(known peers) per message. It is deliberately left unguarded: the
    /// table stays around room size, and measuring before optimizing beats
    /// guessing. `misteval sigbench` cannot see this -- it models the
    /// signaling wire only, not a node's message loop -- so a real
    /// measurement has to come from `miststress` or the `sim` feature. If it
    /// does turn out to matter, the cheap fix is a per-peer "last touched"
    /// timestamp in the implementation and an early return when the previous
    /// refresh is recent relative to `ttl_seconds`; the semantics survive
    /// that, because the refresh only needs to beat the expiry, not track
    /// every packet.
    ///
    /// [`DiscoveryTable`]: crate::signaling::nostr::DiscoveryTable
    /// [`DiscoveryTable::touch_node`]: crate::signaling::nostr::DiscoveryTable::touch_node
    async fn note_peer_alive(&self, _peer: &NodeId) {}

    async fn close(&self) -> Result<()>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait SignalingHandler: HostSendSync {
    async fn handle_message(&self, msg: MessageContent) -> Result<()>;
}
