use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::runtime::Runtime;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
pub use crate::events::*;
pub use crate::logging::*;
use crate::signaling::ws::WebSocketSignaler;
use crate::transports::WebRtcTransport;
pub use mistlib_core::layers::L1Transport;
use mistlib_core::overlay::node_store::NodeStore;
use mistlib_core::overlay::OverlayRouter;
use mistlib_core::signaling::{Signaler, SignalingHandler};
use mistlib_core::transport::Transport;
use mistlib_core::types::NodeId;

mod action;
mod aoi;
mod background;
mod network;
mod run;
mod stats;

pub struct RunningContext {
    pub(crate) transport: Arc<dyn Transport>,
    pub(crate) webrtc_transport: Option<Arc<WebRtcTransport>>,
    /// WebSocket経由で届いたシグナリング（SDP/ICE）の処理先
    pub(crate) ws_signaling_handler: Arc<dyn SignalingHandler>,
    /// WebRTC P2P中継で届いたシグナリングの処理先（中継不使用時はNone）
    pub(crate) p2p_signaling_handler: Option<Arc<dyn SignalingHandler>>,
    pub(crate) signaling_dispatch: Option<Arc<dyn Signaler>>,
    pub(crate) websocket_signaler: Option<Arc<WebSocketSignaler>>,
    pub(crate) l1_transport: Option<Arc<dyn L1Transport>>,
    pub(crate) l1_notifier: Option<Arc<dyn mistlib_core::layers::L1Notifier>>,
    pub(crate) overlay: Option<Arc<OverlayRouter>>,
}

pub enum EngineState {
    Idle,
    Initialized(Arc<RunningContext>),
    Running(Arc<RunningContext>),
}

pub struct MistEngine {
    pub(crate) global_callback: StdMutex<Option<EventCallback>>,
    pub(crate) log_callback: StdMutex<Option<LogCallback>>,
    pub(crate) state: RwLock<EngineState>,
    pub(crate) context_generation: AtomicU64,
    pub(crate) node_store: Arc<StdMutex<NodeStore>>,
    pub(crate) config: StdMutex<Config>,
    pub(crate) runtime: Runtime,
    pub(crate) self_id: StdMutex<NodeId>,
    pub(crate) l0: Arc<crate::layers::native_l0::NativeL0>,
    pub(crate) aoi_nodes: Arc<StdMutex<std::collections::HashSet<NodeId>>>,
    pub(crate) had_connected_peers: AtomicBool,
    pub(crate) all_connections_lost_dispatched: AtomicBool,
    pub(crate) background_loops_cancel: StdMutex<Option<CancellationToken>>,
}

impl Default for MistEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MistEngine {
    pub fn new() -> Self {
        Self {
            global_callback: StdMutex::new(None),
            log_callback: StdMutex::new(None),
            state: RwLock::new(EngineState::Idle),
            context_generation: AtomicU64::new(0),
            node_store: Arc::new(StdMutex::new(NodeStore::new())),
            config: StdMutex::new(Config::new_default()),
            runtime: Runtime::new().expect("Failed to create Tokio runtime"),
            self_id: StdMutex::new(NodeId("local".to_string())),
            l0: Arc::new(crate::layers::native_l0::NativeL0::new()),
            aoi_nodes: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            had_connected_peers: AtomicBool::new(false),
            all_connections_lost_dispatched: AtomicBool::new(false),
            background_loops_cancel: StdMutex::new(None),
        }
    }

    pub(crate) fn ensure_node_registered(&self, node_id: &NodeId) {
        if node_id.is_server() {
            return;
        }
        let mut store = self.node_store.lock().unwrap();
        if !store.nodes.contains_key(node_id) {
            store.update_node_position(
                node_id.clone(),
                mistlib_core::overlay::dnve3::Vector3::zero(),
            );
        }
    }

    pub(crate) fn touch_node(&self, node_id: &NodeId) {
        if node_id.is_server() {
            return;
        }
        self.node_store.lock().unwrap().touch_node(node_id);
    }

    pub async fn get_context(&self) -> Option<Arc<RunningContext>> {
        let state_lock = self.state.read().await;
        match &*state_lock {
            EngineState::Initialized(ctx) => Some(ctx.clone()),
            EngineState::Running(ctx) => Some(ctx.clone()),
            _ => None,
        }
    }
}

pub(crate) fn rust_node_id() -> NodeId {
    NodeId("rust".to_string())
}

pub static ENGINE: std::sync::LazyLock<MistEngine> = std::sync::LazyLock::new(MistEngine::new);

#[cfg(test)]
mod tests;
