use crate::engine::{EngineState, RunningContext, ENGINE};
use crate::signaling::ws::WebSocketSignaler;
use crate::transports::WebRtcTransport;
use async_trait::async_trait;
use mistlib_core::config::Config;
use mistlib_core::layers::L0Engine;
use mistlib_core::overlay::dnve3::strategy::DNVE3Strategy;
use mistlib_core::overlay::OverlayOptimizer;
use mistlib_core::overlay::OverlayTransport;
use mistlib_core::signaling::Signaler;
use mistlib_core::stats::EngineStats;
use mistlib_core::types::NodeId;
use std::sync::Arc;

pub struct NativeL0;

impl NativeL0 {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl L0Engine for NativeL0 {
    fn initialize(&self, local_id: NodeId, signaling_url: String) {
        {
            let mut self_id = ENGINE.self_id.lock().unwrap();
            *self_id = local_id.clone();
        }

        {
            let mut config = ENGINE.config.lock().unwrap();
            config.signaling_url = signaling_url;
        }

        ENGINE.runtime.block_on(async {
            let config = ENGINE.config.lock().unwrap().clone();

            let signaler = Arc::new(WebSocketSignaler::new(&config.signaling_url));
            let relay = Arc::new(crate::signaling::SignalingRelay::new());
            relay
                .set_websocket(signaler.clone() as Arc<dyn Signaler>)
                .await;

            let webrtc_transport = Arc::new(WebRtcTransport::new(
                relay.clone() as Arc<dyn Signaler>,
                local_id.clone(),
            ));
            webrtc_transport.set_max_connections(config.limits.max_connection_count);

            let mut optimizer =
                OverlayOptimizer::new(&config, ENGINE.node_store.clone(), local_id.clone());
            optimizer.add_strategy(Arc::new(DNVE3Strategy::new(
                &config,
                ENGINE.node_store.clone(),
                local_id.clone(),
                optimizer.routing_table.clone(),
            )));

            let optimizer_arc = Arc::new(optimizer);

            struct EngineActionHandler;
            impl mistlib_core::overlay::ActionHandler for EngineActionHandler {
                fn handle_action(&self, action: mistlib_core::action::OverlayAction) {
                    ENGINE.handle_action(action);
                }
            }
            let action_handler = Arc::new(EngineActionHandler);

            let overlay_transport = Arc::new(OverlayTransport {
                optimizer: optimizer_arc.clone(),
                action_handler: action_handler.clone(),
            });

            relay
                .set_overlay(overlay_transport.clone() as Arc<dyn Signaler>)
                .await;

            let l1 = Arc::new(crate::layers::native_l1::NativeL1Transport::new(
                overlay_transport.clone(),
                ENGINE.node_store.clone(),
                local_id.clone(),
            ));

            crate::storage::init_storage(
                overlay_transport.clone() as Arc<dyn mistlib_core::transport::Transport>,
                config.limits.max_connection_count as u64 * 1024 * 1024, 
                None,
            )
            .await;

            let ctx = Arc::new(RunningContext {
                transport: overlay_transport.clone(),
                webrtc_transport: Some(webrtc_transport.clone()),
                signaling_handler: webrtc_transport.clone(),
                websocket_signaler: Some(signaler.clone()),
                l1_transport: Some(l1.clone() as Arc<dyn mistlib_core::layers::L1Transport>),
                l1_notifier: Some(l1.clone() as Arc<dyn mistlib_core::layers::L1Notifier>),
                overlay: Some(optimizer_arc),
            });

            let mut state_lock = ENGINE.state.write().await;
            *state_lock = EngineState::Initialized(ctx);
        });
    }

    fn join_room(&self, room_id: String) {
        let room_id_for_init = room_id.clone();
        ENGINE.runtime.spawn(async move {
            if let Some(ctx) = ENGINE.get_context().await {
                if let Some(wt) = ctx.webrtc_transport.as_ref() {
                    wt.set_room_id(room_id_for_init);
                }
            }
        });

        ENGINE.runtime.spawn(async move {
            let ctx_opt = {
                let state_lock = ENGINE.state.read().await;
                if let EngineState::Initialized(ctx) = &*state_lock {
                    Some(ctx.clone())
                } else {
                    None
                }
            };

            if let Some(ctx) = ctx_opt {
                if let Err(e) = ENGINE.run(ctx).await {
                    tracing::error!("Engine run error: {}", e);
                }
            }
        });

        ENGINE.spawn_background_loops();
    }

    fn leave_room(&self) {
        ENGINE.runtime.spawn(async move {
            let mut state_lock = ENGINE.state.write().await;
            *state_lock = EngineState::Idle;
        });
    }

    fn set_config(&self, config: Config) {
        let mut cfg = ENGINE.config.lock().unwrap();
        *cfg = config;
    }

    fn get_config(&self) -> Config {
        ENGINE.config.lock().unwrap().clone()
    }

    async fn get_stats(&self) -> EngineStats {
        let stats_str = ENGINE.get_stats_json().await;
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
        if let Some(storage) = crate::storage::STORAGE.get() {
            use mistlib_core::layers::L2Storage;
            storage.add(name, data).await
        } else {
            Err(mistlib_core::error::MistError::Internal(
                "Storage not initialized".to_string(),
            ))
        }
    }

    async fn storage_get(&self, cid: &str) -> mistlib_core::error::Result<Vec<u8>> {
        if let Some(storage) = crate::storage::STORAGE.get() {
            use mistlib_core::layers::L2Storage;
            storage.get(cid).await
        } else {
            Err(mistlib_core::error::MistError::Internal(
                "Storage not initialized".to_string(),
            ))
        }
    }
}
