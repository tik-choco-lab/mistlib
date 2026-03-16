use crate::action::OverlayAction;
use crate::config::Config;
use crate::overlay::node_store::NodeStore;
use crate::overlay::ActionHandler;
use crate::overlay::{dnve3::Vector3, OverlayOptimizer};
use crate::runtime::AsyncRuntime;
use crate::signaling::{MessageContent, Signaler, SignalingEnvelope, SignalingHandler};
use crate::transport::{NetworkEvent, NetworkEventHandler, Transport};
use crate::types::NodeId;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct RunningContext {
    pub transport: Arc<dyn Transport>,
    pub network_transport: Option<Arc<dyn Transport>>,
    pub signaling_handler: Arc<dyn SignalingHandler>,
    pub websocket_signaler: Option<Arc<dyn Signaler>>,
    pub overlay: Option<Arc<OverlayOptimizer>>,
}

pub enum EngineState {
    Idle,
    Running(Arc<RunningContext>),
}

pub enum EngineEvent {
    RawMessage(NodeId, Vec<u8>),
    OverlayMessage(NodeId, Vec<u8>),
    NeighborsUpdated(Vec<u8>),
    AoiEntered(NodeId),
    AoiLeft(NodeId),
}

pub trait EngineEventHandler: Send + Sync {
    fn on_event(&self, event: EngineEvent);
}

struct DummyEngineEventHandler;
impl EngineEventHandler for DummyEngineEventHandler {
    fn on_event(&self, _event: EngineEvent) {}
}

pub struct MistEngine {
    pub self_id: Arc<Mutex<NodeId>>,
    pub config: Arc<Mutex<Config>>,
    pub node_store: Arc<Mutex<NodeStore>>,
    pub state: Arc<Mutex<EngineState>>,
    pub runtime: Arc<dyn AsyncRuntime>,
    pub aoi_nodes: Arc<Mutex<std::collections::HashSet<NodeId>>>,
    event_handler: Arc<Mutex<Arc<dyn EngineEventHandler>>>,
}

impl ActionHandler for MistEngine {
    fn handle_action(&self, action: OverlayAction) {
        let state_arc = self.state.clone();

        self.runtime.spawn(Box::pin(async move {
            let ctx = {
                let lock = state_arc.lock().unwrap();
                if let EngineState::Running(c) = &*lock {
                    Some(c.clone())
                } else {
                    None
                }
            };
            if let Some(ctx) = ctx {
                match action {
                    OverlayAction::SendMessage { to, data, method } => {
                        if let Some(nt) = &ctx.network_transport {
                            if to.0.is_empty() {
                                let _ = nt.broadcast(data, method).await;
                            } else {
                                let _ = nt.send(&to, data, method).await;
                            }
                        } else {
                            if to.0.is_empty() {
                                let _ = ctx.transport.broadcast(data, method).await;
                            } else {
                                let _ = ctx.transport.send(&to, data, method).await;
                            }
                        }
                    }
                    OverlayAction::Connect { to } => {
                        if let Some(nt) = &ctx.network_transport {
                            let _ = nt.connect(&to).await;
                        } else {
                            let _ = ctx.transport.connect(&to).await;
                        }
                    }
                    OverlayAction::Disconnect { to } => {
                        if let Some(nt) = &ctx.network_transport {
                            let _ = nt.disconnect(&to).await;
                        } else {
                            let _ = ctx.transport.disconnect(&to).await;
                        }
                    }
                    OverlayAction::SendSignaling { to, envelope } => {
                        if let Some(sig) = &ctx.websocket_signaler {
                            let _ = sig.send_signaling(&to, envelope.content).await;
                        }
                    }
                }
            }
        }));
    }
}

impl MistEngine {
    pub fn new(runtime: Arc<dyn AsyncRuntime>) -> Arc<Self> {
        Arc::new(Self {
            self_id: Arc::new(Mutex::new(NodeId("local".to_string()))),
            config: Arc::new(Mutex::new(Config::new_default())),
            node_store: Arc::new(Mutex::new(NodeStore::new())),
            state: Arc::new(Mutex::new(EngineState::Idle)),
            runtime,
            aoi_nodes: Arc::new(Mutex::new(std::collections::HashSet::new())),
            event_handler: Arc::new(Mutex::new(Arc::new(DummyEngineEventHandler))),
        })
    }

    pub fn set_event_handler(&self, handler: Arc<dyn EngineEventHandler>) {
        let mut lock = self.event_handler.lock().unwrap();
        *lock = handler;
    }

    pub fn leave_room(&self) {
        let mut state = self.state.lock().unwrap();
        *state = EngineState::Idle;
        let mut store = self.node_store.lock().unwrap();
        store.nodes.clear();
        store.last_updated.clear();
    }

    pub async fn run(
        self: Arc<Self>,
        ctx: RunningContext,
        mut sig_rx: mpsc::UnboundedReceiver<MessageContent>,
    ) -> Result<(), String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<NetworkEvent>();

        let ctx_arc = Arc::new(ctx);
        {
            let mut state = self.state.lock().unwrap();
            *state = EngineState::Running(ctx_arc.clone());
        }

        struct Adapter(mpsc::UnboundedSender<NetworkEvent>);
        impl NetworkEventHandler for Adapter {
            fn on_event(&self, event: NetworkEvent) {
                let _ = self.0.send(event);
            }
        }

        if let Some(nt) = &ctx_arc.network_transport {
            nt.start(Arc::new(Adapter(tx.clone())))
                .await
                .map_err(|e| e.to_string())?;
        }

        ctx_arc
            .transport
            .start(Arc::new(Adapter(tx)))
            .await
            .map_err(|e| e.to_string())?;

        let ctx_sig = ctx_arc.clone();
        let runtime_sig = self.runtime.clone();
        runtime_sig.spawn(Box::pin(async move {
            while let Some(msg) = sig_rx.recv().await {
                let _ = ctx_sig.signaling_handler.handle_message(msg).await;
            }
        }));

        let self_net = self.clone();
        let runtime_net = self.runtime.clone();
        runtime_net.spawn(Box::pin(async move {
            while let Some(event) = rx.recv().await {
                self_net.process_network_event(event).await;
            }
        }));

        let self_tick = self.clone();
        let runtime_tick = self.runtime.clone();
        runtime_tick.spawn(Box::pin(async move {
            loop {
                self_tick
                    .runtime
                    .sleep(web_time::Duration::from_millis(1000))
                    .await;
                self_tick.tick().await;
            }
        }));

        Ok(())
    }

    async fn process_network_event(&self, event: NetworkEvent) {
        let from_origin = event.from.clone();
        tracing::debug!("MistEngine: Received network event from {}, len={}", from_origin.0, event.data.len());
        
        match bincode::deserialize::<SignalingEnvelope>(&event.data) {
            Ok(envelope) => {
                let (to_self, _self_id_val) = {
                    let id_lock = self.self_id.lock().unwrap();
                    (envelope.to == *id_lock || envelope.to.0.is_empty(), id_lock.clone())
                };
                
                tracing::debug!("MistEngine: Deserialized envelope from={}, to={}, to_self={}", envelope.from.0, envelope.to.0, to_self);
                
                let content = envelope.content.clone();

                let overlay_opt = {
                    let state = self.state.lock().unwrap();
                    if let EngineState::Running(c) = &*state {
                        c.overlay.clone()
                    } else {
                        None
                    }
                };

                let actions = if let Some(ov) = overlay_opt {
                    ov.handle_envelope(envelope, from_origin.clone())
                } else {
                    vec![]
                };

                for action in actions {
                    self.handle_action(action);
                }

                if to_self {
                    match content {
                        MessageContent::Raw(payload) => {
                            tracing::debug!("MistEngine: Dispatching RawMessage to handler, payload_len={}", payload.len());
                            let handler = self.event_handler.lock().unwrap().clone();
                            handler.on_event(EngineEvent::RawMessage(from_origin, payload));
                        }
                        MessageContent::Overlay(msg) => {
                            tracing::debug!("MistEngine: Dispatching OverlayMessage type={}", msg.message_type);
                            if msg.message_type == 100 {
                                if let Ok(data) =
                                    serde_json::from_slice::<serde_json::Value>(&msg.payload)
                                {
                                    if data["type"] == "sync_pos" {
                                        if let (Some(x), Some(y), Some(z)) = (
                                            data["position"]["x"].as_f64(),
                                            data["position"]["y"].as_f64(),
                                            data["position"]["z"].as_f64(),
                                        ) {
                                            tracing::debug!("MistEngine: Updating node {} position to ({}, {}, {})", from_origin.0, x, y, z);
                                            let mut store = self.node_store.lock().unwrap();
                                            store.update_node_position(
                                                from_origin.clone(),
                                                Vector3::new(x as f32, y as f32, z as f32),
                                            );
                                        }
                                    }
                                }
                            }
                            let handler = self.event_handler.lock().unwrap().clone();
                            handler.on_event(EngineEvent::OverlayMessage(from_origin, msg.payload));
                        }
                        MessageContent::Data(sig_data) => {
                            tracing::debug!("MistEngine: Handling signaling Data from {} for {}", sig_data.sender_id.0, sig_data.receiver_id.0);
                            {
                                let mut store = self.node_store.lock().unwrap();
                                if !store.nodes.contains_key(&sig_data.sender_id)
                                    && sig_data.sender_id.0 != "server"
                                {
                                    store.update_node_position(
                                        sig_data.sender_id.clone(),
                                        Vector3::new(0.0, 0.0, 0.0),
                                    );
                                }
                            }
                            let ctx = {
                                let state = self.state.lock().unwrap();
                                if let EngineState::Running(c) = &*state {
                                    Some(c.clone())
                                } else {
                                    None
                                }
                            };
                            if let Some(c) = ctx {
                                let handler = c.signaling_handler.clone();
                                self.runtime.spawn(Box::pin(async move {
                                    let _ = handler
                                        .handle_message(MessageContent::Data(sig_data))
                                        .await;
                                }));
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::debug!("MistEngine: Deserialization failed for event from {}: {:?}. Forwarding as raw data.", from_origin.0, e);
                let handler = self.event_handler.lock().unwrap().clone();
                handler.on_event(EngineEvent::RawMessage(from_origin, event.data.to_vec()));
            }
        }
    }

    async fn tick(&self) {
        let (connected_ids, states) = {
            let state = self.state.lock().unwrap();
            if let EngineState::Running(ctx) = &*state {
                if let Some(ov) = &ctx.overlay {
                    let s = if let Some(nt) = &ctx.network_transport {
                        nt.get_active_connection_states()
                    } else {
                        ctx.transport.get_active_connection_states()
                    };

                    let conn: std::collections::HashSet<_> = s
                        .iter()
                        .filter(|(_, st)| *st == crate::types::ConnectionState::Connected)
                        .map(|(id, _)| id.clone())
                        .collect();

                    {
                        let mut rt = ov.routing_table.lock().unwrap();
                        let current_rt_connected = rt.connected_nodes.clone();
                        for id in &conn {
                            rt.on_connected(id.clone());
                        }
                        for id in current_rt_connected {
                            if !conn.contains(&id) {
                                rt.on_disconnected(&id);
                            }
                        }
                    }

                    (conn, s)
                } else {
                    (std::collections::HashSet::new(), vec![])
                }
            } else {
                (std::collections::HashSet::new(), vec![])
            }
        };

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

        let handler = self.event_handler.lock().unwrap().clone();
        for id in aoi_entered {
            handler.on_event(EngineEvent::AoiEntered(id));
        }
        for id in aoi_left {
            handler.on_event(EngineEvent::AoiLeft(id));
        }

        let nodes = {
            let store = self.node_store.lock().unwrap();
            store.get_connected_nodes_json(&connected_ids).into_bytes()
        };

        if !nodes.is_empty() {
            let handler = self.event_handler.lock().unwrap().clone();
            handler.on_event(EngineEvent::NeighborsUpdated(nodes));
        }

        let actions = {
            let state = self.state.lock().unwrap();
            if let EngineState::Running(ctx) = &*state {
                if let Some(ov) = &ctx.overlay {
                    let config = self.config.lock().unwrap().clone();
                    ov.tick(&config, &states)
                } else {
                    vec![]
                }
            } else {
                vec![]
            }
        };

        for action in actions {
            self.handle_action(action);
        }
    }
}
