use crate::app::{WasmEngineEventHandler, ENGINE, L1_TRANSPORT, PENDING_START, WEBRTC_TRANSPORT};
use crate::layers::wasm_l1::WasmL1Transport;
use crate::signaling::relay::SignalingRelay;
use crate::signaling::ws::WasmWebSocketSignaler;
use crate::transport::webrtc::WasmWebRtcTransport;
use async_trait::async_trait;
use mistlib_core::config::Config;
use mistlib_core::engine::RunningContext;
use mistlib_core::layers::L0Engine;
use mistlib_core::overlay::dnve3::strategy::DNVE3Strategy;
use mistlib_core::overlay::OverlayOptimizer;
use mistlib_core::signaling::{MessageContent, Signaler};
use mistlib_core::stats::EngineStats;
use mistlib_core::types::NodeId;
use std::sync::Arc;
use tokio::sync::mpsc;
use wasm_bindgen_futures::spawn_local;

thread_local! {
    static TARGET_ROOM: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
}

pub struct WasmL0;

impl WasmL0 {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait(?Send)]
impl L0Engine for WasmL0 {
    fn initialize(&self, local_id: NodeId, signaling_url: String) {
        ENGINE.with(|e| {
            *e.self_id.lock().unwrap() = local_id.clone();
            e.set_event_handler(Arc::new(WasmEngineEventHandler));
        });

        let config = ENGINE.with(|e| {
            let mut config_lock = e.config.lock().unwrap();
            config_lock.signaling_url = signaling_url.clone();
            config_lock.clone()
        });

        spawn_local(async move {
            let signaler = Arc::new(WasmWebSocketSignaler::new(&signaling_url));
            let relay = Arc::new(SignalingRelay::new());
            relay
                .set_delegate(signaler.clone() as Arc<dyn Signaler>)
                .await;

            let webrtc = Arc::new(WasmWebRtcTransport::new(
                relay.clone() as Arc<dyn Signaler>,
                local_id.clone(),
            ));
            webrtc.set_max_connections(config.limits.max_connection_count);

            let mut optimizer = OverlayOptimizer::new(
                &config,
                ENGINE.with(|e| e.node_store.clone()),
                local_id.clone(),
            );
            optimizer.add_strategy(Arc::new(DNVE3Strategy::new(
                &config,
                ENGINE.with(|e| e.node_store.clone()),
                local_id.clone(),
                optimizer.routing_table.clone(),
            )));
            let optimizer_arc = Arc::new(optimizer);

            struct EngineActionHandler(Arc<mistlib_core::engine::MistEngine>);
            impl mistlib_core::overlay::ActionHandler for EngineActionHandler {
                fn handle_action(&self, action: mistlib_core::action::OverlayAction) {
                    self.0.handle_action(action);
                }
            }
            let action_handler = Arc::new(EngineActionHandler(ENGINE.with(|e| e.clone())));

            optimizer_arc
                .start(
                    Arc::new(crate::runtime::WasmRuntime),
                    Arc::new(config),
                    action_handler.clone(),
                )
                .await;

            let overlay_transport = Arc::new(mistlib_core::overlay::OverlayTransport {
                optimizer: optimizer_arc.clone(),
                action_handler: action_handler.clone(),
            });

            relay
                .set_overlay(overlay_transport.clone() as Arc<dyn Signaler>)
                .await;

            let l1 = Arc::new(WasmL1Transport::new(
                overlay_transport.clone() as Arc<dyn mistlib_core::transport::Transport>,
                ENGINE.with(|e| e.node_store.clone()),
                local_id.clone(),
            ));

            L1_TRANSPORT.with(|t| *t.borrow_mut() = Some(l1.clone()));

            let ctx = RunningContext {
                transport: overlay_transport.clone(),
                network_transport: Some(
                    webrtc.clone() as Arc<dyn mistlib_core::transport::Transport>
                ),
                signaling_handler: webrtc.clone(),
                websocket_signaler: Some(signaler.clone()),
                overlay: Some(optimizer_arc),
            };

            crate::storage::init_storage(
                overlay_transport.clone() as Arc<dyn mistlib_core::transport::Transport>,
                None,
            );

            let (sig_tx, sig_rx) = mpsc::unbounded_channel::<MessageContent>();
            let _ = signaler.connect(sig_tx).await;

            WEBRTC_TRANSPORT.with(|t| *t.borrow_mut() = Some(webrtc.clone()));
            PENDING_START.with(|p| *p.borrow_mut() = Some((ctx, sig_rx)));

            let auto_join = TARGET_ROOM.with(|r| r.borrow_mut().take());
            if let Some(room) = auto_join {
                crate::app::L0.with(|l0| l0.join_room(room));
            }
        });
    }

    fn join_room(&self, room_id: String) {
        let mut is_running = false;
        let mut started = false;
        WEBRTC_TRANSPORT.with(|t| {
            if let Some(webrtc) = t.borrow().as_ref() {
                webrtc.set_room_id(room_id.clone());
                is_running = true;
            }
        });

        let pending = PENDING_START.with(|p| p.borrow_mut().take());
        if let Some((ctx, sig_rx)) = pending {
            let engine_arc = ENGINE.with(|e| e.clone());
            spawn_local(async move {
                let _ = engine_arc.run(ctx, sig_rx).await;
            });
            started = true;
        } else if is_running {
            WEBRTC_TRANSPORT.with(|t| {
                if let Some(webrtc) = t.borrow().as_ref() {
                    let webrtc_clone = webrtc.clone();
                    spawn_local(async move {
                        let _ = webrtc_clone.request_peers().await;
                    });
                }
            });
        }

        if !started && !is_running {
            TARGET_ROOM.with(|r| *r.borrow_mut() = Some(room_id));
        }
    }

    fn leave_room(&self) {
        ENGINE.with(|e| e.leave_room());
    }

    fn set_config(&self, config: Config) {
        ENGINE.with(|e| {
            let mut lock = e.config.lock().unwrap();
            *lock = config;
        });
    }

    fn get_config(&self) -> Config {
        ENGINE.with(|e| e.config.lock().unwrap().clone())
    }

    async fn get_stats(&self) -> EngineStats {
        let stats_str = crate::app::get_stats();
        serde_json::from_str(&stats_str).unwrap_or_else(|_| EngineStats {
            message_count: 0,
            send_bits: 0,
            receive_bits: 0,
            rtt_millis: std::collections::HashMap::new(),
            eval_send_bits: 0,
            eval_receive_bits: 0,
            eval_message_count: 0,
            nodes: vec![],
            diag_peers: 0,
            diag_connection_states: 0,
            diag_pending_candidates: 0,
        })
    }

    async fn storage_add(&self, name: &str, data: &[u8]) -> mistlib_core::error::Result<String> {
        crate::storage::storage_add(name.to_string(), data)
            .await
            .map_err(|e| {
                mistlib_core::error::MistError::Internal(
                    e.as_string()
                        .unwrap_or_else(|| "Unknown storage error".to_string()),
                )
            })
    }

    async fn storage_get(&self, cid: &str) -> mistlib_core::error::Result<Vec<u8>> {
        crate::storage::storage_get(cid.to_string())
            .await
            .map_err(|e| {
                mistlib_core::error::MistError::Internal(
                    e.as_string()
                        .unwrap_or_else(|| "Unknown storage error".to_string()),
                )
            })
            .map(|arr| arr.to_vec())
    }
}
