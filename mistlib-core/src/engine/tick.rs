use super::{EngineEvent, EngineState, MistEngine};
use crate::action::OverlayAction;
use crate::overlay::ActionHandler;
use crate::types::{ConnectionState, NodeId};
use std::collections::HashSet;

impl MistEngine {
    pub(super) async fn tick(&self) {
        // Lock ordering: state -> config -> node_store -> self_id -> aoi_nodes.
        let (connected_ids, states) = self.step_connections();

        let (expire_duration, aoi_range) = {
            let cfg = self.config.lock().expect("config lock poisoned");
            (
                web_time::Duration::from_secs_f32(cfg.limits.expire_node_seconds.max(0.0)),
                cfg.dnve.aoi_range,
            )
        };

        let (aoi_entered, aoi_left) = self.step_aoi(aoi_range, expire_duration, &connected_ids);
        for id in aoi_entered {
            self.emit(EngineEvent::AoiEntered(id));
        }
        for id in aoi_left {
            self.emit(EngineEvent::AoiLeft(id));
        }

        let aoi_nodes_json = {
            let store = self.node_store.lock().expect("node_store lock poisoned");
            let aoi = self.aoi_nodes.lock().expect("aoi_nodes lock poisoned");
            store.get_nodes_json(&aoi).into_bytes()
        };
        self.emit(EngineEvent::AoiNodesUpdated(aoi_nodes_json));

        let nodes = {
            let store = self.node_store.lock().expect("node_store lock poisoned");
            store.get_connected_nodes_json(&connected_ids).into_bytes()
        };
        if !nodes.is_empty() {
            self.emit(EngineEvent::NeighborsUpdated(nodes));
        }

        for action in self.step_overlay_tick(&states) {
            self.handle_action(action);
        }
    }

    /// Collects the current connected set and syncs the routing table.
    fn step_connections(&self) -> (HashSet<NodeId>, Vec<(NodeId, ConnectionState)>) {
        let state = self.state.lock().expect("state lock poisoned");
        let EngineState::Running(ctx) = &*state else {
            return (HashSet::new(), vec![]);
        };
        let Some(ov) = &ctx.overlay else {
            return (HashSet::new(), vec![]);
        };

        let states = ctx.active_connection_states();
        let connected = ov.sync_connection_states(&states);
        (connected, states)
    }

    /// Expires stale nodes and computes which nodes entered/left the AOI.
    fn step_aoi(
        &self,
        aoi_range: f32,
        expire_duration: web_time::Duration,
        connected_ids: &HashSet<NodeId>,
    ) -> (Vec<NodeId>, Vec<NodeId>) {
        let mut store = self.node_store.lock().expect("node_store lock poisoned");
        for id in connected_ids {
            store.touch_node(id);
        }
        store.retain_recent(expire_duration);

        let self_id = self.self_id.lock().expect("self_id lock poisoned").clone();
        let current_aoi = store.get_nodes_in_range(&self_id, aoi_range);

        let mut aoi_lock = self.aoi_nodes.lock().expect("aoi_nodes lock poisoned");
        let entered = current_aoi
            .iter()
            .filter(|id| !aoi_lock.contains(*id))
            .cloned()
            .collect();
        let left = aoi_lock
            .iter()
            .filter(|id| !current_aoi.contains(*id))
            .cloned()
            .collect();
        *aoi_lock = current_aoi;
        (entered, left)
    }

    /// Runs the overlay tick and returns the resulting actions.
    fn step_overlay_tick(&self, states: &[(NodeId, ConnectionState)]) -> Vec<OverlayAction> {
        let state = self.state.lock().expect("state lock poisoned");
        let EngineState::Running(ctx) = &*state else {
            return vec![];
        };
        let Some(ov) = &ctx.overlay else {
            return vec![];
        };
        let config = self.config.lock().expect("config lock poisoned").clone();
        ov.tick(&config, states)
    }
}
