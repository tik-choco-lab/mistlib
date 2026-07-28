use super::*;
use crate::action::OverlayAction;
use crate::config::Config;
use crate::overlay::{ActionHandler, OverlayRouter};
use crate::signaling::{SignalingData, SignalingType};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Default)]
struct RecordingSignaler {
    sent: Mutex<Vec<NodeId>>,
    resets: AtomicUsize,
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Signaler for RecordingSignaler {
    async fn send_signaling(&self, to: &NodeId, _msg: MessageContent) -> crate::error::Result<()> {
        self.sent.lock().unwrap().push(to.clone());
        Ok(())
    }

    async fn reset_session(&self) -> crate::error::Result<()> {
        self.resets.fetch_add(1, Ordering::SeqCst);
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

#[derive(Default)]
struct RecordingSignalingHandler {
    handled: Mutex<Vec<MessageContent>>,
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl SignalingHandler for RecordingSignalingHandler {
    async fn handle_message(&self, msg: MessageContent) -> crate::error::Result<()> {
        self.handled.lock().unwrap().push(msg);
        Ok(())
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
    bootstrap: Arc<RecordingSignaler>,
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
    (Arc::new(RoutedSignaler::new(bootstrap, overlay)), router)
}

#[test]
fn server_signaling_uses_bootstrap_route() {
    let handler = Arc::new(RecordingActionHandler::default());
    let bootstrap = Arc::new(RecordingSignaler::default());
    let (relay, _router) = make_relay(handler.clone(), bootstrap.clone());

    futures::executor::block_on(relay.send_signaling(
        &NodeId("server".to_string()),
        signaling_msg("local", "server"),
    ))
    .unwrap();

    assert_eq!(
        bootstrap.sent.lock().unwrap().as_slice(),
        &[NodeId("server".to_string())]
    );
    assert!(handler.actions.lock().unwrap().is_empty());
}

#[test]
fn broadcast_signaling_uses_bootstrap_route() {
    let handler = Arc::new(RecordingActionHandler::default());
    let bootstrap = Arc::new(RecordingSignaler::default());
    let (relay, _router) = make_relay(handler.clone(), bootstrap.clone());

    futures::executor::block_on(
        relay.send_signaling(&NodeId::broadcast(), signaling_msg("local", "")),
    )
    .unwrap();

    assert_eq!(
        bootstrap.sent.lock().unwrap().as_slice(),
        &[NodeId::broadcast()]
    );
    assert!(handler.actions.lock().unwrap().is_empty());
}

#[test]
fn bootstrap_ingress_peer_uses_bootstrap_response_path() {
    let handler = Arc::new(RecordingActionHandler::default());
    let bootstrap = Arc::new(RecordingSignaler::default());
    let (relay, _router) = make_relay(handler.clone(), bootstrap.clone());
    relay.remember_route(&NodeId("peer-a".to_string()), SignalingRoute::WebSocket);

    futures::executor::block_on(relay.send_signaling(
        &NodeId("peer-a".to_string()),
        signaling_msg("local", "peer-a"),
    ))
    .unwrap();

    assert_eq!(
        bootstrap.sent.lock().unwrap().as_slice(),
        &[NodeId("peer-a".to_string())]
    );
    assert!(handler.actions.lock().unwrap().is_empty());
}

#[test]
fn overlay_route_overrides_stale_bootstrap_ingress_route() {
    let handler = Arc::new(RecordingActionHandler::default());
    let bootstrap = Arc::new(RecordingSignaler::default());
    let (relay, router) = make_relay(handler.clone(), bootstrap.clone());
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

    assert!(bootstrap.sent.lock().unwrap().is_empty());
    assert!(matches!(
        handler.actions.lock().unwrap().as_slice(),
        [OverlayAction::SendMessage { to, .. }] if *to == NodeId("peer-a".to_string())
    ));
}

#[test]
fn overlay_ingress_peer_uses_overlay_response_path() {
    let handler = Arc::new(RecordingActionHandler::default());
    let bootstrap = Arc::new(RecordingSignaler::default());
    let (relay, router) = make_relay(handler.clone(), bootstrap.clone());
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

    assert!(bootstrap.sent.lock().unwrap().is_empty());
    assert!(matches!(
        handler.actions.lock().unwrap().as_slice(),
        [OverlayAction::SendMessage { to, .. }] if *to == NodeId("peer-a".to_string())
    ));
}

#[test]
fn routed_handler_records_ingress_route_before_forwarding_message() {
    let action_handler = Arc::new(RecordingActionHandler::default());
    let bootstrap = Arc::new(RecordingSignaler::default());
    let (relay, _router) = make_relay(action_handler, bootstrap);
    let inner = Arc::new(RecordingSignalingHandler::default());
    let handler =
        RoutedSignalingHandler::new(relay.clone(), inner.clone(), SignalingRoute::WebSocket);

    let msg = signaling_msg("peer-a", "local");
    futures::executor::block_on(handler.handle_message(msg)).unwrap();

    assert_eq!(
        relay.route_for(&NodeId("peer-a".to_string())),
        Some(SignalingRoute::WebSocket)
    );
    assert_eq!(inner.handled.lock().unwrap().len(), 1);
}

/// Reproduces the reconnect-flap root cause: a peer that has *previously*
/// exchanged signaling over the overlay (its direct WebRTC connection) but
/// whose live overlay route is currently missing -- e.g. the peer's
/// connection just dropped and is mid-reconnect, or it was just
/// re-established and the routing table's connected-node set hasn't been
/// resynced by the next periodic tick yet (`MistEngine::tick`, ~1s cadence).
/// Before the fix, this hit the same `None => overlay` arm as a genuinely
/// unknown peer and failed with `RouteNotFound`, silently dropping the exact
/// Offer/Answer/ICE-restart message needed to complete or recover this
/// peer's own connection -- wedging it until a full peer teardown occurred.
/// `to` is always a direct signaling counterpart here (never a third node
/// being relayed through someone else), so falling back to the always-on
/// bootstrap WebSocket is safe and self-limited to this reconnect window.
#[test]
fn stale_overlay_route_without_live_connection_falls_back_to_bootstrap() {
    let handler = Arc::new(RecordingActionHandler::default());
    let bootstrap = Arc::new(RecordingSignaler::default());
    let (relay, router) = make_relay(handler.clone(), bootstrap.clone());
    let peer = NodeId("peer-a".to_string());

    // Simulate history: the peer was a live overlay neighbor before (so its
    // route got remembered as Overlay), then disconnected -- `on_disconnected`
    // clears it from `connected_nodes`, but `RoutedSignaler`'s own per-peer
    // route memory is untouched by that (no link between the two today).
    router
        .routing_table
        .lock()
        .unwrap()
        .on_connected(peer.clone());
    relay.remember_route(&peer, SignalingRoute::Overlay);
    router.routing_table.lock().unwrap().on_disconnected(&peer);

    futures::executor::block_on(relay.send_signaling(&peer, signaling_msg("local", "peer-a")))
        .expect("signaling to a known peer must not be dropped just because its overlay route is momentarily stale");

    assert_eq!(bootstrap.sent.lock().unwrap().as_slice(), &[peer]);
    assert!(
        handler.actions.lock().unwrap().is_empty(),
        "must not attempt to send over the (routeless) overlay path"
    );
}

#[test]
fn peer_without_recorded_route_defaults_to_overlay_without_bootstrap_fallback() {
    let handler = Arc::new(RecordingActionHandler::default());
    let bootstrap = Arc::new(RecordingSignaler::default());
    let (relay, _router) = make_relay(handler.clone(), bootstrap.clone());

    let err = futures::executor::block_on(relay.send_signaling(
        &NodeId("peer-a".to_string()),
        signaling_msg("local", "peer-a"),
    ))
    .unwrap_err();

    assert!(matches!(err, crate::error::MistError::RouteNotFound(_)));
    assert!(bootstrap.sent.lock().unwrap().is_empty());
    assert!(handler.actions.lock().unwrap().is_empty());
}

#[test]
fn reset_session_clears_recorded_routes_and_resets_bootstrap() {
    let handler = Arc::new(RecordingActionHandler::default());
    let bootstrap = Arc::new(RecordingSignaler::default());
    let (relay, _router) = make_relay(handler, bootstrap.clone());
    let peer = NodeId("peer-a".to_string());
    relay.remember_route(&peer, SignalingRoute::WebSocket);

    futures::executor::block_on(relay.reset_session()).unwrap();

    assert_eq!(relay.route_for(&peer), None);
    assert_eq!(bootstrap.resets.load(Ordering::SeqCst), 1);
}
