use crate::engine::{EngineState, RunningContext, ENGINE};
use crate::signaling::ws::WebSocketSignaler;
use crate::transports::WebRtcTransport;
use mistlib_core::config::Config;
use mistlib_core::overlay::dnve3::strategy::DNVE3Strategy;
use mistlib_core::overlay::OverlayRouter;
use mistlib_core::overlay::OverlayTransport;
use mistlib_core::signaling::{
    RoutedSignaler, RoutedSignalingHandler, Signaler, SignalingHandler, SignalingRoute,
};
use mistlib_core::types::NodeId;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub(super) fn initialize(local_id: NodeId, signaling_url: String) {
    set_local_id(local_id.clone());
    set_signaling_url(signaling_url);
    invalidate_context();
    ENGINE.runtime.block_on(build_context(local_id));
}

async fn build_context(local_id: NodeId) {
    let config = ENGINE.config.lock().unwrap().clone();
    let signaler = build_signaler(&config);
    let (overlay_transport, router) = build_overlay_transport(&config, &local_id);
    let routed_signaler = Arc::new(RoutedSignaler::new(
        signaler.clone() as Arc<dyn Signaler>,
        overlay_transport.clone(),
    ));
    let webrtc_transport = build_webrtc_transport(&routed_signaler, &local_id, &config);
    let ws_signaling_handler = Arc::new(RoutedSignalingHandler::new(
        routed_signaler.clone(),
        webrtc_transport.clone() as Arc<dyn SignalingHandler>,
        SignalingRoute::WebSocket,
    ));
    let p2p_signaling_handler = Arc::new(RoutedSignalingHandler::new(
        routed_signaler.clone(),
        webrtc_transport.clone() as Arc<dyn SignalingHandler>,
        SignalingRoute::Overlay,
    ));
    let l1 = build_l1_transport(&overlay_transport, &local_id);
    init_storage(&overlay_transport, &config).await;

    let ctx = Arc::new(RunningContext {
        transport: overlay_transport.clone(),
        webrtc_transport: Some(webrtc_transport.clone()),
        ws_signaling_handler,
        p2p_signaling_handler: Some(p2p_signaling_handler),
        signaling_dispatch: Some(overlay_transport.clone() as Arc<dyn Signaler>),
        websocket_signaler: Some(signaler),
        l1_transport: Some(l1.clone() as Arc<dyn mistlib_core::layers::L1Transport>),
        l1_notifier: Some(l1 as Arc<dyn mistlib_core::layers::L1Notifier>),
        overlay: Some(router),
    });

    let mut state_lock = ENGINE.state.write().await;
    *state_lock = EngineState::Initialized(ctx);
}

fn set_local_id(local_id: NodeId) {
    *ENGINE.self_id.lock().unwrap() = local_id;
}

fn set_signaling_url(url: String) {
    ENGINE.config.lock().unwrap().signaling_url = url;
}

fn invalidate_context() {
    ENGINE.context_generation.fetch_add(1, Ordering::Relaxed);
}

fn build_signaler(config: &Config) -> Arc<WebSocketSignaler> {
    Arc::new(WebSocketSignaler::new(&config.signaling_url))
}

fn build_webrtc_transport(
    signaler: &Arc<RoutedSignaler>,
    local_id: &NodeId,
    config: &Config,
) -> Arc<WebRtcTransport> {
    let transport = Arc::new(WebRtcTransport::new(
        signaler.clone() as Arc<dyn Signaler>,
        local_id.clone(),
    ));
    transport.set_max_connections(config.limits.max_connection_count);
    transport
}

fn build_overlay_transport(
    config: &Config,
    local_id: &NodeId,
) -> (Arc<OverlayTransport>, Arc<OverlayRouter>) {
    let mut router = OverlayRouter::new(config, ENGINE.node_store.clone(), local_id.clone());
    router.add_strategy(Arc::new(DNVE3Strategy::new(
        config,
        ENGINE.node_store.clone(),
        local_id.clone(),
        router.routing_table.clone(),
    )));
    let router_arc = Arc::new(router);

    struct EngineActionHandler;
    impl mistlib_core::overlay::ActionHandler for EngineActionHandler {
        fn handle_action(&self, action: mistlib_core::action::OverlayAction) {
            ENGINE.handle_action(action);
        }
    }

    let transport = Arc::new(OverlayTransport {
        router: router_arc.clone(),
        action_handler: Arc::new(EngineActionHandler),
    });
    (transport, router_arc)
}

fn build_l1_transport(
    overlay_transport: &Arc<OverlayTransport>,
    local_id: &NodeId,
) -> Arc<crate::layers::native_l1::NativeL1Transport> {
    Arc::new(crate::layers::native_l1::NativeL1Transport::new(
        overlay_transport.clone(),
        ENGINE.node_store.clone(),
        local_id.clone(),
    ))
}

async fn init_storage(overlay_transport: &Arc<OverlayTransport>, config: &Config) {
    crate::storage::init_storage(
        overlay_transport.clone() as Arc<dyn mistlib_core::transport::Transport>,
        config.limits.max_connection_count as u64 * 1024 * 1024,
        None,
    )
    .await;
}
