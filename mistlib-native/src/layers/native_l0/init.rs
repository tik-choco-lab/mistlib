use crate::engine::{SessionCtx, ENGINE};
use crate::signaling::{BootstrapSignaler, NostrSignaler, WebSocketSignaler};
use crate::transports::WebRtcTransport;
use mistlib_core::config::{Config, SignalingMode};
use mistlib_core::overlay::dnve3::strategy::DNVE3Strategy;
use mistlib_core::overlay::node_store::NodeStore;
use mistlib_core::overlay::OverlayRouter;
use mistlib_core::overlay::OverlayTransport;
use mistlib_core::signaling::{
    RoutedSignaler, RoutedSignalingHandler, Signaler, SignalingHandler, SignalingRoute,
};
use mistlib_core::types::NodeId;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio_util::sync::CancellationToken;

/// `init`/`init_with_config` only set `self_id`/`Config` now (SPEC-15):
/// building a session stack moved to `join_room` (see `build_session_context`
/// below), so a room isn't constructed -- let alone started -- until someone
/// actually joins one.
pub(super) fn initialize(local_id: NodeId, signaling_url: String) {
    set_local_id(local_id);
    set_signaling_url(signaling_url);
    ENGINE.initialized.store(true, Ordering::Relaxed);
}

fn set_local_id(local_id: NodeId) {
    *ENGINE.self_id.lock().unwrap() = local_id;
}

fn set_signaling_url(url: String) {
    ENGINE.config.lock().unwrap().signaling_url = url;
}

/// Builds one room's full session stack (signaler, WebRTC transport,
/// overlay/DNVE3, node store) but does not start or announce it. The caller
/// (`room::join_room`) inserts the result into the session registry and then
/// calls `ENGINE.run(ctx)`.
pub(super) async fn build_session_context(room_id: String, local_id: NodeId) -> Arc<SessionCtx> {
    let config = ENGINE.config.lock().unwrap().clone();
    let node_store = Arc::new(StdMutex::new(NodeStore::new()));

    let signaler = build_signaler(&config, &local_id);
    let (overlay_transport, router) =
        build_overlay_transport(&config, &local_id, &node_store, room_id.clone());
    let routed_signaler = Arc::new(RoutedSignaler::new(
        signaler.clone() as Arc<dyn Signaler>,
        overlay_transport.clone(),
    ));
    let webrtc_transport =
        build_webrtc_transport(&routed_signaler, &local_id, &config, room_id.clone());
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
    let l1 = build_l1_transport(&overlay_transport, &node_store, &local_id, room_id.clone());

    // Storage is a process-wide singleton shared by every room (SPEC-15 rule
    // 8, blocks are content-addressed so sharing is correct dedup); the
    // OnceCell guard inside makes repeat calls (one per room join) a cheap no-op.
    init_storage(&config).await;

    Arc::new(SessionCtx {
        room_id,
        transport: overlay_transport.clone(),
        webrtc_transport: Some(webrtc_transport),
        ws_signaling_handler,
        p2p_signaling_handler: Some(p2p_signaling_handler),
        signaling_dispatch: Some(overlay_transport as Arc<dyn Signaler>),
        bootstrap_signaler: Some(signaler),
        l1_transport: Some(l1.clone() as Arc<dyn mistlib_core::layers::L1Transport>),
        l1_notifier: Some(l1 as Arc<dyn mistlib_core::layers::L1Notifier>),
        overlay: Some(router),
        node_store,
        aoi_nodes: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        had_connected_peers: AtomicBool::new(false),
        all_connections_lost_dispatched: AtomicBool::new(false),
        cancel: CancellationToken::new(),
    })
}

fn build_signaler(config: &Config, local_id: &NodeId) -> Arc<BootstrapSignaler> {
    match config.signaling.mode {
        SignalingMode::WebSocket => Arc::new(BootstrapSignaler::WebSocket(Arc::new(
            WebSocketSignaler::new(&config.signaling_url),
        ))),
        SignalingMode::Nostr => {
            let nostr_config = config
                .signaling
                .nostr
                .clone()
                .expect("Nostr signaling mode requires validated Nostr config");
            Arc::new(BootstrapSignaler::Nostr(Arc::new(NostrSignaler::new(
                local_id.clone(),
                nostr_config,
            ))))
        }
    }
}

fn build_webrtc_transport(
    signaler: &Arc<RoutedSignaler>,
    local_id: &NodeId,
    config: &Config,
    room_id: String,
) -> Arc<WebRtcTransport> {
    let transport = Arc::new(WebRtcTransport::new(
        signaler.clone() as Arc<dyn Signaler>,
        local_id.clone(),
    ));
    transport.set_room_id(room_id);
    transport.set_max_connections(config.limits.max_connection_count);
    transport.set_max_message_bytes(config.limits.max_message_bytes);
    transport.set_ice_servers(crate::transports::webrtc::map_ice_servers(
        &config.webrtc.ice_servers,
    ));
    transport
}

fn build_overlay_transport(
    config: &Config,
    local_id: &NodeId,
    node_store: &Arc<StdMutex<NodeStore>>,
    room_id: String,
) -> (Arc<OverlayTransport>, Arc<OverlayRouter>) {
    let mut router = OverlayRouter::new(config, node_store.clone(), local_id.clone());
    router.add_strategy(Arc::new(DNVE3Strategy::new(
        config,
        node_store.clone(),
        local_id.clone(),
        router.routing_table.clone(),
    )));
    let router_arc = Arc::new(router);

    // mistlib-core's `ActionHandler` trait has no room parameter, so this
    // session's own room_id has to be carried by the handler instance itself
    // -- it resolves back to this session via `ENGINE.handle_action_in_room`.
    struct SessionActionHandler {
        room_id: String,
    }
    impl mistlib_core::overlay::ActionHandler for SessionActionHandler {
        fn handle_action(&self, action: mistlib_core::action::OverlayAction) {
            ENGINE.handle_action_in_room(self.room_id.clone(), action);
        }
    }

    let transport = Arc::new(OverlayTransport {
        router: router_arc.clone(),
        action_handler: Arc::new(SessionActionHandler { room_id }),
    });
    (transport, router_arc)
}

fn build_l1_transport(
    overlay_transport: &Arc<OverlayTransport>,
    node_store: &Arc<StdMutex<NodeStore>>,
    local_id: &NodeId,
    room_id: String,
) -> Arc<crate::layers::native_l1::NativeL1Transport> {
    Arc::new(crate::layers::native_l1::NativeL1Transport::new(
        overlay_transport.clone(),
        node_store.clone(),
        local_id.clone(),
        room_id,
    ))
}

async fn init_storage(config: &Config) {
    crate::storage::init_storage(&config.storage, None).await;
}
