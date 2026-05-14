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
    websocket: Arc<dyn Signaler>,
    overlay: Arc<OverlayTransport>,
    peer_routes: Mutex<HashMap<NodeId, SignalingRoute>>,
}

impl RoutedSignaler {
    pub fn new(websocket: Arc<dyn Signaler>, overlay: Arc<OverlayTransport>) -> Self {
        Self {
            websocket,
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
            return self.websocket.send_signaling(to, msg).await;
        }

        if self.overlay.has_signaling_route(to) {
            return self.overlay.send_signaling(to, msg).await;
        }

        match self.route_for(to) {
            Some(SignalingRoute::WebSocket) => self.websocket.send_signaling(to, msg).await,
            Some(SignalingRoute::Overlay) | None => self.overlay.send_signaling(to, msg).await,
        }
    }

    async fn close(&self) -> crate::error::Result<()> {
        self.websocket.close().await
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
        }
        self.inner.handle_message(msg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::OverlayAction;
    use crate::config::Config;
    use crate::overlay::{ActionHandler, OverlayRouter};
    use crate::signaling::{SignalingData, SignalingType};
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSignaler {
        sent: Mutex<Vec<NodeId>>,
    }

    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    impl Signaler for RecordingSignaler {
        async fn send_signaling(
            &self,
            to: &NodeId,
            _msg: MessageContent,
        ) -> crate::error::Result<()> {
            self.sent.lock().unwrap().push(to.clone());
            Ok(())
        }

        async fn close(&self) -> crate::error::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingActionHandler {
        actions: Mutex<Vec<OverlayAction>>,
    }

    impl ActionHandler for RecordingActionHandler {
        fn handle_action(&self, action: OverlayAction) {
            self.actions.lock().unwrap().push(action);
        }
    }

    fn signaling_msg(from: &str, to: &str) -> MessageContent {
        MessageContent::Data(SignalingData {
            sender_id: NodeId(from.to_string()),
            receiver_id: NodeId(to.to_string()),
            room_id: "room".to_string(),
            data: "{}".to_string(),
            signaling_type: SignalingType::Offer,
        })
    }

    fn make_relay(
        handler: Arc<RecordingActionHandler>,
        websocket: Arc<RecordingSignaler>,
    ) -> (Arc<RoutedSignaler>, Arc<OverlayRouter>) {
        let router = Arc::new(OverlayRouter::new(
            &Config::new_default(),
            Arc::new(Mutex::new(crate::overlay::node_store::NodeStore::new())),
            NodeId("local".to_string()),
        ));
        let overlay = Arc::new(OverlayTransport {
            router: router.clone(),
            action_handler: handler,
        });
        (Arc::new(RoutedSignaler::new(websocket, overlay)), router)
    }

    #[test]
    fn server_signaling_uses_websocket_for_bootstrap() {
        let handler = Arc::new(RecordingActionHandler::default());
        let websocket = Arc::new(RecordingSignaler::default());
        let (relay, _router) = make_relay(handler.clone(), websocket.clone());

        futures::executor::block_on(relay.send_signaling(
            &NodeId("server".to_string()),
            signaling_msg("local", "server"),
        ))
        .unwrap();

        assert_eq!(
            websocket.sent.lock().unwrap().as_slice(),
            &[NodeId("server".to_string())]
        );
        assert!(handler.actions.lock().unwrap().is_empty());
    }

    #[test]
    fn websocket_ingress_peer_uses_websocket_response_path() {
        let handler = Arc::new(RecordingActionHandler::default());
        let websocket = Arc::new(RecordingSignaler::default());
        let (relay, _router) = make_relay(handler.clone(), websocket.clone());
        relay.remember_route(&NodeId("peer-a".to_string()), SignalingRoute::WebSocket);

        futures::executor::block_on(relay.send_signaling(
            &NodeId("peer-a".to_string()),
            signaling_msg("local", "peer-a"),
        ))
        .unwrap();

        assert_eq!(
            websocket.sent.lock().unwrap().as_slice(),
            &[NodeId("peer-a".to_string())]
        );
        assert!(handler.actions.lock().unwrap().is_empty());
    }

    #[test]
    fn overlay_route_overrides_stale_websocket_ingress_route() {
        let handler = Arc::new(RecordingActionHandler::default());
        let websocket = Arc::new(RecordingSignaler::default());
        let (relay, router) = make_relay(handler.clone(), websocket.clone());
        relay.remember_route(&NodeId("peer-a".to_string()), SignalingRoute::WebSocket);
        router
            .routing_table
            .lock()
            .unwrap()
            .on_connected(NodeId("peer-a".to_string()));

        futures::executor::block_on(relay.send_signaling(
            &NodeId("peer-a".to_string()),
            signaling_msg("local", "peer-a"),
        ))
        .unwrap();

        assert!(websocket.sent.lock().unwrap().is_empty());
        assert!(matches!(
            handler.actions.lock().unwrap().as_slice(),
            [OverlayAction::SendMessage { to, .. }] if *to == NodeId("peer-a".to_string())
        ));
    }

    #[test]
    fn overlay_ingress_peer_uses_overlay_response_path() {
        let handler = Arc::new(RecordingActionHandler::default());
        let websocket = Arc::new(RecordingSignaler::default());
        let (relay, router) = make_relay(handler.clone(), websocket.clone());
        router
            .routing_table
            .lock()
            .unwrap()
            .on_connected(NodeId("peer-a".to_string()));
        relay.remember_route(&NodeId("peer-a".to_string()), SignalingRoute::Overlay);

        futures::executor::block_on(relay.send_signaling(
            &NodeId("peer-a".to_string()),
            signaling_msg("local", "peer-a"),
        ))
        .unwrap();

        assert!(websocket.sent.lock().unwrap().is_empty());
        assert!(matches!(
            handler.actions.lock().unwrap().as_slice(),
            [OverlayAction::SendMessage { to, .. }] if *to == NodeId("peer-a".to_string())
        ));
    }

    #[test]
    fn peer_without_recorded_route_defaults_to_overlay_without_websocket_fallback() {
        let handler = Arc::new(RecordingActionHandler::default());
        let websocket = Arc::new(RecordingSignaler::default());
        let (relay, _router) = make_relay(handler.clone(), websocket.clone());

        let err = futures::executor::block_on(relay.send_signaling(
            &NodeId("peer-a".to_string()),
            signaling_msg("local", "peer-a"),
        ))
        .unwrap_err();

        assert!(matches!(err, crate::error::MistError::RouteNotFound(_)));
        assert!(websocket.sent.lock().unwrap().is_empty());
        assert!(handler.actions.lock().unwrap().is_empty());
    }
}
