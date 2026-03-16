use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::runtime::Runtime;
use tokio::sync::RwLock;

use crate::config::Config;
pub use crate::events::*;
pub use crate::logging::*;
use crate::runtime::TokioRuntime;
use crate::signaling::ws::WebSocketSignaler;
use crate::transports::WebRtcTransport;
use mistlib_core::action::OverlayAction;
pub use mistlib_core::layers::L1Transport;
use mistlib_core::overlay::node_store::NodeStore;
use mistlib_core::overlay::{ActionHandler, OverlayOptimizer};
use mistlib_core::signaling::{MessageContent, SignalingHandler};
use mistlib_core::transport::{NetworkEvent, Transport};
use mistlib_core::types::NodeId;
use tokio::sync::mpsc;

pub struct RunningContext {
    pub transport: Arc<dyn Transport>,
    pub webrtc_transport: Option<Arc<WebRtcTransport>>,
    pub signaling_handler: Arc<dyn SignalingHandler>,
    pub websocket_signaler: Option<Arc<WebSocketSignaler>>,
    pub l1_transport: Option<Arc<dyn L1Transport>>,
    pub l1_notifier: Option<Arc<dyn mistlib_core::layers::L1Notifier>>,

    pub overlay: Option<Arc<OverlayOptimizer>>,
}

pub enum EngineState {
    Idle,
    Initialized(Arc<RunningContext>),
    Running(Arc<RunningContext>),
}

use mistlib_core::stats::{EngineStats, NodeStats, SctpStats};

pub struct MistEngine {
    pub global_callback: StdMutex<Option<EventCallback>>,
    pub log_callback: StdMutex<Option<LogCallback>>,
    pub state: RwLock<EngineState>,
    pub node_store: Arc<StdMutex<NodeStore>>,
    pub config: StdMutex<Config>,
    pub runtime: Runtime,
    pub self_id: StdMutex<NodeId>,
    pub l0: Arc<crate::layers::native_l0::NativeL0>,
    pub aoi_nodes: Arc<StdMutex<std::collections::HashSet<NodeId>>>,
}

impl mistlib_core::overlay::ActionHandler for MistEngine {
    fn handle_action(&self, action: OverlayAction) {
        let handle = self.runtime.handle().clone();
        handle.spawn(async move {
            let state_lock = ENGINE.state.read().await;
            if let EngineState::Running(ctx) = &*state_lock {
                match action {
                    OverlayAction::SendMessage { to, data, method } => {
                        if let Some(wt) = &ctx.webrtc_transport {
                            let _ = wt.send(&to, data, method).await;
                        }
                    }
                    OverlayAction::Connect { to } => {
                        if let Some(wt) = &ctx.webrtc_transport {
                            let _ = wt.connect(&to).await;
                        }
                    }
                    OverlayAction::Disconnect { to } => {
                        if let Some(wt) = &ctx.webrtc_transport {
                            let _ = wt.disconnect(&to).await;
                        }
                    }
                    OverlayAction::SendSignaling { to, envelope } => {
                        if let Some(ws) = &ctx.websocket_signaler {
                            use mistlib_core::signaling::Signaler;
                            let _ = ws.send_signaling(&to, envelope.content).await;
                        }
                    }
                }
            }
        });
    }
}

impl MistEngine {
    pub fn new() -> Self {
        Self {
            global_callback: StdMutex::new(None),
            log_callback: StdMutex::new(None),
            state: RwLock::new(EngineState::Idle),
            node_store: Arc::new(StdMutex::new(NodeStore::new())),
            config: StdMutex::new(Config::new_default()),
            runtime: Runtime::new().expect("Failed to create Tokio runtime"),
            self_id: StdMutex::new(NodeId("local".to_string())),
            l0: Arc::new(crate::layers::native_l0::NativeL0::new()),
            aoi_nodes: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        }
    }

    pub async fn run(&self, ctx: Arc<RunningContext>) -> crate::error::Result<()> {
        let (tx, rx) = mpsc::channel::<NetworkEvent>(8192);
        let (sig_tx, sig_rx) = mpsc::channel::<MessageContent>(1024);

        {
            let mut state_lock = self.state.write().await;
            *state_lock = EngineState::Running(ctx.clone());
        }

        if let Some(ws) = ctx.websocket_signaler.as_ref() {
            ws.connect(sig_tx).await?;
        }

        struct NetworkEventHandlerAdapter(mpsc::Sender<NetworkEvent>);
        impl mistlib_core::transport::NetworkEventHandler for NetworkEventHandlerAdapter {
            fn on_event(&self, event: NetworkEvent) {
                let _ = self.0.try_send(event);
            }
        }

        let adapter = Arc::new(NetworkEventHandlerAdapter(tx));

        if let Some(wt) = ctx.webrtc_transport.as_ref() {
            wt.start(adapter.clone())
                .await
                .map_err(|e| crate::error::MistError::Core(e))?;
        }

        ctx.transport
            .start(adapter)
            .await
            .map_err(|e| crate::error::MistError::Core(e))?;

        if let Some(ov) = ctx.overlay.as_ref() {
            let config = Arc::new(self.config.lock().unwrap().clone());
            let runtime = Arc::new(TokioRuntime::new(self.runtime.handle().clone()));
            struct DummyHandler;
            impl mistlib_core::overlay::ActionHandler for DummyHandler {
                fn handle_action(&self, _action: OverlayAction) {}
            }
            ov.start(runtime, config, Arc::new(DummyHandler)).await;
        }

        self.spawn_signaling_loop(sig_rx);
        self.process_network_events(rx).await;

        Ok(())
    }

    fn spawn_signaling_loop(&self, mut sig_rx: mpsc::Receiver<MessageContent>) {
        let runtime_handle = self.runtime.handle().clone();
        runtime_handle.spawn(async move {
            while let Some(msg) = sig_rx.recv().await {
                let state_lock = ENGINE.state.read().await;
                if let EngineState::Running(ctx) = &*state_lock {
                    let _ = ctx.signaling_handler.handle_message(msg).await;
                }
            }
        });
    }

    async fn process_network_events(&self, mut rx: mpsc::Receiver<NetworkEvent>) {
        while let Some(event) = rx.recv().await {
            let ctx = {
                let state_lock = self.state.read().await;
                match &*state_lock {
                    EngineState::Running(ctx) => ctx.clone(),
                    _ => continue,
                }
            };

            let Some(ov) = ctx.overlay.as_ref() else {
                continue;
            };

            let from_origin = event.from.clone();
            match bincode::deserialize::<mistlib_core::signaling::SignalingEnvelope>(&event.data) {
                Ok(envelope) => {
                    let to_self =
                        envelope.to == *self.self_id.lock().unwrap() || envelope.to.0.is_empty();
                    let content = envelope.content.clone();

                    let actions = ov.handle_envelope(envelope, from_origin.clone());
                    for action in actions {
                        use mistlib_core::overlay::ActionHandler;
                        self.handle_action(action);
                    }

                    if to_self {
                        match content {
                            MessageContent::Raw(_payload) => {
                                dispatch_event(EVENT_RAW, &from_origin, &_payload);
                            }
                            MessageContent::Overlay(overlay_msg) => {
                                dispatch_event(EVENT_OVERLAY, &from_origin, &overlay_msg.payload);
                            }
                            MessageContent::Data(signaling_data) => {
                                let handler = ctx.signaling_handler.clone();
                                self.runtime.handle().spawn(async move {
                                    let _ = handler
                                        .handle_message(MessageContent::Data(signaling_data))
                                        .await;
                                });
                            }
                        }
                    }
                }
                Err(_) => {
                    
                    
                    use crate::storage::resolver;
                    if let Some(cid) = resolver::parse_want_message(&event.data) {
                        let from = from_origin.clone();
                        self.runtime.handle().spawn(async move {
                            crate::storage::handle_want(from, cid).await;
                        });
                    } else if let Some(cid) = resolver::parse_query_message(&event.data) {
                        let from = from_origin.clone();
                        self.runtime.handle().spawn(async move {
                            crate::storage::handle_query(from, cid).await;
                        });
                    } else if let Some(cid) = resolver::parse_have_status_message(&event.data) {
                        crate::storage::handle_have_status(from_origin, cid);
                    } else if let Some((cid, data)) = resolver::parse_have_message(&event.data) {
                        crate::storage::handle_have(cid, data);
                    }
                }
            }
        }
    }

    pub fn spawn_background_loops(&self) {
        self.runtime.spawn(async move {
            let mut interval = tokio::time::interval(web_time::Duration::from_millis(1000));
            loop {
                interval.tick().await;

                ENGINE.check_and_dispatch_aoi().await;
                ENGINE.check_and_dispatch_neighbors().await;
            }
        });

        self.runtime.spawn(async move {
            let mut interval = tokio::time::interval(web_time::Duration::from_millis(1000));
            loop {
                interval.tick().await;

                let actions = {
                    let state_lock = ENGINE.state.read().await;
                    match &*state_lock {
                        EngineState::Running(ctx) => {
                            if let (Some(ov), Some(wt)) = (&ctx.overlay, &ctx.webrtc_transport) {
                                let states = wt.get_active_connection_states();
                                let config = ENGINE.config.lock().unwrap().clone();
                                ov.tick(&config, &states)
                            } else {
                                vec![]
                            }
                        }
                        _ => vec![],
                    }
                };

                for action in actions {
                    ENGINE.handle_action(action);
                }
            }
        });
    }

    pub async fn check_and_dispatch_aoi(&self) {
        let (aoi_entered, aoi_left) = {
            let mut store = self.node_store.lock().unwrap();
            let expire_duration = {
                let cfg = self.config.lock().unwrap();
                web_time::Duration::from_secs(cfg.limits.expire_node_seconds as u64)
            };
            store.retain_recent(expire_duration);

            let cfg = self.config.lock().unwrap();
            let aoi_range = cfg.dnve.aoi_range;

            let self_id = self.self_id.lock().unwrap().clone();
            let current_aoi = store.get_nodes_in_range(&self_id, aoi_range);

            let mut aoi_lock = self.aoi_nodes.lock().unwrap();
            let mut entered = Vec::new();
            let mut left = Vec::new();

            for id in &current_aoi {
                if !aoi_lock.contains(id) {
                    entered.push(id.clone());
                }
            }
            for id in &*aoi_lock {
                if !current_aoi.contains(id) {
                    left.push(id.clone());
                }
            }
            *aoi_lock = current_aoi;
            (entered, left)
        };

        for id in aoi_entered {
            dispatch_event(EVENT_AOI_ENTERED, &id, b"entered");
        }
        for id in aoi_left {
            dispatch_event(EVENT_AOI_LEFT, &id, b"left");
        }

        let state_lock = self.state.read().await;
        if let EngineState::Running(ctx) = &*state_lock {
            if let Some(l1) = ctx.l1_notifier.as_ref() {
                let store = self.node_store.lock().unwrap();
                let aoi_lock = self.aoi_nodes.lock().unwrap();
                for id in &*aoi_lock {
                    if let Some(n) = store.nodes.get(id) {
                        l1.notify_node_position_updated(
                            id,
                            n.position.x,
                            n.position.y,
                            n.position.z,
                        );
                    }
                }
            }
        }
    }

    pub async fn check_and_dispatch_neighbors(&self) {
        let connected_ids = {
            let state_lock = self.state.read().await;
            if let EngineState::Running(ctx) = &*state_lock {
                if let Some(overlay) = &ctx.overlay {
                    let rt = overlay.routing_table.lock().unwrap();
                    rt.connected_nodes.clone()
                } else {
                    std::collections::HashSet::new()
                }
            } else {
                std::collections::HashSet::new()
            }
        };

        let nodes = {
            let store = self.node_store.lock().unwrap();
            store.get_connected_nodes_json(&connected_ids).into_bytes()
        };

        if !nodes.is_empty() {
            dispatch_event(EVENT_NEIGHBORS, &NodeId("rust".to_string()), &nodes);
        }
    }

    pub async fn get_context(&self) -> Option<Arc<RunningContext>> {
        let state_lock = self.state.read().await;
        match &*state_lock {
            EngineState::Initialized(ctx) => Some(ctx.clone()),
            EngineState::Running(ctx) => Some(ctx.clone()),
            _ => None,
        }
    }

    pub async fn get_stats_json(&self) -> String {
        let snapshot = mistlib_core::stats::STATS.snapshot_and_reset();

        let rtt_millis: std::collections::HashMap<String, f32> = snapshot
            .rtt_millis
            .into_iter()
            .map(|(k, v)| (k.0, v))
            .collect();

        let t_lock = std::time::Instant::now();
        let wt_ref = {
            let state_lock = self.state.read().await;
            match &*state_lock {
                EngineState::Initialized(ctx) | EngineState::Running(ctx) => {
                    ctx.webrtc_transport.clone()
                }
                _ => None,
            }
        };
        let lock_ms = t_lock.elapsed().as_millis();

        let t_sctp = std::time::Instant::now();
        let (sctp_raw, diag_peers, diag_connection_states, diag_pending_candidates) =
            if let Some(wt) = wt_ref.as_ref() {
                let stats_future = async {
                    let sctp = wt.get_sctp_stats().await;
                    let peers = wt.peers.read().await.len();
                    let states = wt.connection_states.read().unwrap().len();
                    let cands = wt.pending_candidates.read().await.len();
                    (sctp, peers, states, cands)
                };
                match tokio::time::timeout(std::time::Duration::from_millis(500), stats_future)
                    .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        
                        let states = wt.connection_states.read().unwrap().len();
                        tracing::error!(
                            "[Diag] get_stats_json TIMEOUT (500ms) lock={}ms conn_states={}",
                            lock_ms,
                            states
                        );
                        (std::collections::HashMap::new(), 0, states, 0)
                    }
                }
            } else {
                (std::collections::HashMap::new(), 0, 0, 0)
            };
        let sctp_ms = t_sctp.elapsed().as_millis();

        if lock_ms > 50 || sctp_ms > 100 {
            tracing::error!(
                "[Diag] get_stats_json slow: lock={}ms sctp={}ms peers={} conn_states={}",
                lock_ms,
                sctp_ms,
                diag_peers,
                diag_connection_states
            );
        }

        let nodes = sctp_raw
            .into_iter()
            .map(|(node_id, s)| {
                let loss = 0;
                NodeStats {
                    id: node_id,
                    sctp_stats: SctpStats {
                        messages_sent: s.messages_sent,
                        messages_received: s.messages_received,
                        bytes_sent: s.bytes_sent,
                        bytes_received: s.bytes_received,
                        estimated_packet_loss: loss,
                        state: format!("{:?}", s.state).to_lowercase(),
                    },
                }
            })
            .collect();

        let stats = EngineStats {
            message_count: snapshot.message_count,
            send_bits: snapshot.send_bits,
            receive_bits: snapshot.receive_bits,
            rtt_millis,
            eval_send_bits: snapshot.eval_send_bits,
            eval_receive_bits: snapshot.eval_receive_bits,
            eval_message_count: snapshot.eval_message_count,
            nodes,
            diag_peers,
            diag_connection_states,
            diag_pending_candidates,
        };

        match serde_json::to_string(&stats) {
            Ok(json) => json,
            Err(e) => {
                let nan_count = stats
                    .rtt_millis
                    .values()
                    .filter(|v| v.is_nan() || v.is_infinite())
                    .count();
                let mut fixed = stats;
                fixed
                    .rtt_millis
                    .retain(|_, v| !v.is_nan() && !v.is_infinite());
                let fallback = format!(
                    r#"{{"diagPeers":{},"diagConnectionStates":{},"diagPendingCandidates":{},"diagSerdeError":"{}","diagNanCount":{}}}"#,
                    fixed.diag_peers,
                    fixed.diag_connection_states,
                    fixed.diag_pending_candidates,
                    e,
                    nan_count
                );
                fallback
            }
        }
    }
}

lazy_static::lazy_static! {
    pub static ref ENGINE: MistEngine = MistEngine::new();
}

#[cfg(test)]
mod tests;
