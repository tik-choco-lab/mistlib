use crate::overlay::OverlayTransport;
use crate::signaling::{MessageContent, Signaler, SignalingHandler};
use crate::types::NodeId;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalingRoute {
    WebSocket,
    Overlay,
}

pub struct RoutedSignaler {
    bootstrap: Arc<dyn Signaler>,
    overlay: Arc<OverlayTransport>,
    peer_routes: Mutex<HashMap<NodeId, SignalingRoute>>,
}

impl RoutedSignaler {
    pub fn new(bootstrap: Arc<dyn Signaler>, overlay: Arc<OverlayTransport>) -> Self {
        Self {
            bootstrap,
            overlay,
            peer_routes: Mutex::new(HashMap::new()),
        }
    }

    pub fn remember_route(&self, peer: &NodeId, route: SignalingRoute) {
        if peer.is_server() || peer.is_broadcast() {
            return;
        }
        self.peer_routes
            .lock()
            .expect("signaling route lock poisoned")
            .insert(peer.clone(), route);
    }

    pub fn route_for(&self, peer: &NodeId) -> Option<SignalingRoute> {
        self.peer_routes
            .lock()
            .expect("signaling route lock poisoned")
            .get(peer)
            .copied()
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Signaler for RoutedSignaler {
    async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> crate::error::Result<()> {
        if to.is_server() || to.is_broadcast() {
            return self.bootstrap.send_signaling(to, msg).await;
        }

        if self.overlay.has_signaling_route(to) {
            return self.overlay.send_signaling(to, msg).await;
        }

        match self.route_for(to) {
            Some(SignalingRoute::WebSocket) => self.bootstrap.send_signaling(to, msg).await,
            // `Some(Overlay)` means overlay signaling worked for this peer at
            // some point in the past, not that it's live right now (we
            // already know it isn't -- `has_signaling_route` above just
            // returned false). That gap is routine and short-lived: the
            // routing table's connected-node set is refreshed by a periodic
            // tick (see `MistEngine::tick`, ~1s cadence) rather than the
            // instant a peer's transport connection comes up, and it is
            // briefly empty again while that same peer is mid-reconnect.
            // `to` here is always a direct WebRTC signaling counterpart (an
            // Offer/Answer/Candidate/Request target), never a third node
            // being relayed through someone else, so bootstrap WebSocket is
            // guaranteed reachable for it (every such peer was introduced via
            // a WebSocket `Request`/`Offer` before any overlay route could
            // ever have been recorded for it -- see `remember_route`).
            // Falling back here turns "silently drop the exact signaling
            // needed to recover this peer's own connection" into "briefly use
            // the always-available bootstrap channel until the overlay route
            // resyncs" -- self-limited to the reconnect window, not a general
            // WebSocket fallback for live overlay routing.
            //
            // A peer with no recorded route at all (`None`) is different: it
            // may be a brand-new node discovered purely through the overlay
            // mesh (e.g. the cascade-distribution relay path) that we have
            // never directly exchanged signaling with, so it keeps the
            // original no-silent-fallback behavior and fails with
            // `RouteNotFound` when overlay has no next hop.
            Some(SignalingRoute::Overlay) => self.bootstrap.send_signaling(to, msg).await,
            None => self.overlay.send_signaling(to, msg).await,
        }
    }

    /// Forwarded to the bootstrap signaler, which is the one holding expiring
    /// per-peer state. The overlay keeps none, so there is nothing to refresh
    /// on that side.
    async fn note_peer_alive(&self, peer: &NodeId) {
        self.bootstrap.note_peer_alive(peer).await;
    }

    async fn reset_session(&self) -> crate::error::Result<()> {
        self.peer_routes
            .lock()
            .expect("signaling route lock poisoned")
            .clear();
        self.bootstrap.reset_session().await
    }

    async fn close(&self) -> crate::error::Result<()> {
        self.bootstrap.close().await
    }
}

pub struct RoutedSignalingHandler {
    routes: Arc<RoutedSignaler>,
    inner: Arc<dyn SignalingHandler>,
    ingress: SignalingRoute,
}

impl RoutedSignalingHandler {
    pub fn new(
        routes: Arc<RoutedSignaler>,
        inner: Arc<dyn SignalingHandler>,
        ingress: SignalingRoute,
    ) -> Self {
        Self {
            routes,
            inner,
            ingress,
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl SignalingHandler for RoutedSignalingHandler {
    async fn handle_message(&self, msg: MessageContent) -> crate::error::Result<()> {
        if let MessageContent::Data(data) = &msg {
            self.routes.remember_route(&data.sender_id, self.ingress);
            // Deliberately unconditional, not `if self.ingress == Overlay`.
            // This is the one place both ingresses meet, and the bug this
            // fixes was itself an ingress the refresh path had missed; adding
            // another "this branch only" condition here would invite the same
            // omission for the next transport. On the WebSocket ingress it
            // duplicates the refresh the Nostr handler already does, which is
            // harmless -- `touch_node` takes a `max()` of the existing expiry.
            self.routes.note_peer_alive(&data.sender_id).await;
        }
        self.inner.handle_message(msg).await
    }
}

#[cfg(test)]
mod tests;
