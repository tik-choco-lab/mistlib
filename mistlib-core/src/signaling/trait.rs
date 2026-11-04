use super::message::MessageContent;
use crate::error::Result;
use crate::types::{HostSendSync, NodeId, SessionReestablishedHook};
use async_trait::async_trait;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait Signaler: HostSendSync {
    async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> Result<()>;
    /// Called only after a transport reaches Connected, never for signaling traffic.
    fn note_peer_connected(&self, _peer: &NodeId) {}

    /// True only when this send currently has an overlay next hop. A remembered
    /// ingress or merely having some connected peer does not suffice.
    fn has_overlay_route(&self, _peer: &NodeId) -> bool {
        false
    }

    /// A live binding token changes when fresh discovery/Request evidence arrives.
    /// Legacy signalers have no expiring binding table.
    async fn peer_retry_token(&self, _peer: &NodeId) -> Option<String> {
        Some(String::new())
    }

    /// Ask the local transport dispatcher to call connect for a known peer.
    /// This is local-only; it never synthesizes a relay event.
    async fn retry_peer(&self, _peer: &NodeId) -> Result<()> {
        Err(crate::error::MistError::Signaling(
            "local retry unavailable".into(),
        ))
    }

    /// Local transport teardown invalidates queued negotiation retries.
    fn end_connection_attempt(&self, _peer: &NodeId) {}

    async fn reset_session(&self) -> Result<()> {
        Ok(())
    }

    /// Adds diagnostic context without changing the reset operation.
    async fn reset_session_with_reason(&self, _reason: &str) -> Result<()> {
        self.reset_session().await
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

    /// Periodic bootstrap maintenance with the transport's actual connected
    /// peer count. Default is inert; Nostr uses it for bounded alternate probes.
    async fn maintain_bootstrap(&self, _connected_peers: usize) {}

    async fn close(&self) -> Result<()>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait SignalingHandler: HostSendSync {
    async fn handle_message(&self, msg: MessageContent) -> Result<()>;
}
