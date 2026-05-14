use std::sync::atomic::Ordering;

use super::{
    dispatch_event, EVENT_ALL_CONNECTIONS_LOST, EVENT_AOI_ENTERED, EVENT_AOI_LEFT, EVENT_AOI_NODES,
    EVENT_NEIGHBORS,
};
use super::{rust_node_id, EngineState};

impl super::MistEngine {
    pub(crate) fn reset_connection_loss_tracking(&self) {
        self.had_connected_peers.store(false, Ordering::Relaxed);
        self.all_connections_lost_dispatched
            .store(false, Ordering::Relaxed);
    }

    fn should_dispatch_all_connections_lost(&self, connected_count: usize) -> bool {
        if connected_count > 0 {
            self.had_connected_peers.store(true, Ordering::Relaxed);
            self.all_connections_lost_dispatched
                .store(false, Ordering::Relaxed);
            return false;
        }

        if self.had_connected_peers.load(Ordering::Relaxed)
            && !self.all_connections_lost_dispatched.load(Ordering::Relaxed)
        {
            self.all_connections_lost_dispatched
                .store(true, Ordering::Relaxed);
            return true;
        }

        false
    }

    pub async fn check_and_dispatch_aoi(&self) {
        let connected_ids = {
            let state_lock = self.state.read().await;
            match &*state_lock {
                EngineState::Running(ctx) => ctx
                    .overlay
                    .as_ref()
                    .map(|overlay| {
                        overlay
                            .routing_table
                            .lock()
                            .unwrap()
                            .connected_nodes
                            .clone()
                    })
                    .unwrap_or_default(),
                _ => std::collections::HashSet::new(),
            }
        };

        let (aoi_entered, aoi_left) = {
            let mut store = self.node_store.lock().unwrap();
            let expire_duration = {
                let cfg = self.config.lock().unwrap();
                web_time::Duration::from_secs_f32(cfg.limits.expire_node_seconds.max(0.0))
            };
            for id in &connected_ids {
                store.touch_node(id);
            }
            store.retain_recent(expire_duration);

            let cfg = self.config.lock().unwrap();
            let aoi_range = cfg.dnve.aoi_range;

            let self_id = self.self_id.lock().unwrap().clone();
            let current_aoi = store.get_nodes_in_range(&self_id, aoi_range);

            let mut aoi_lock = self.aoi_nodes.lock().unwrap();
            let entered: Vec<_> = current_aoi
                .iter()
                .filter(|id| !aoi_lock.contains(*id))
                .cloned()
                .collect();
            let left: Vec<_> = aoi_lock
                .iter()
                .filter(|id| !current_aoi.contains(*id))
                .cloned()
                .collect();
            *aoi_lock = current_aoi;
            (entered, left)
        };

        for id in aoi_entered {
            dispatch_event(EVENT_AOI_ENTERED, &id, b"entered");
        }
        for id in aoi_left {
            dispatch_event(EVENT_AOI_LEFT, &id, b"left");
        }

        {
            let store = self.node_store.lock().unwrap();
            let aoi_lock = self.aoi_nodes.lock().unwrap();
            let json = store.get_nodes_json(&aoi_lock).into_bytes();
            dispatch_event(EVENT_AOI_NODES, &rust_node_id(), &json);
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
            match &*state_lock {
                EngineState::Running(ctx) => {
                    if let Some(overlay) = &ctx.overlay {
                        let rt = overlay.routing_table.lock().unwrap();
                        rt.connected_nodes.clone()
                    } else {
                        std::collections::HashSet::new()
                    }
                }
                _ => std::collections::HashSet::new(),
            }
        };

        if self.should_dispatch_all_connections_lost(connected_ids.len()) {
            let from = self.self_id.lock().unwrap().clone();
            let payload = serde_json::json!({
                "reason": "allConnectionsLost",
                "connectedCount": connected_ids.len(),
            })
            .to_string()
            .into_bytes();
            dispatch_event(EVENT_ALL_CONNECTIONS_LOST, &from, &payload);
        }

        let nodes = {
            let store = self.node_store.lock().unwrap();
            store.get_connected_nodes_json(&connected_ids).into_bytes()
        };

        if !nodes.is_empty() {
            dispatch_event(EVENT_NEIGHBORS, &rust_node_id(), &nodes);
        }
    }
}
