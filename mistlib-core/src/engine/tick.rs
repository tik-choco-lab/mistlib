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

        // Native also reports every AOI node position each tick, even unchanged.
        let positions: Vec<_> = {
            let store = self.node_store.lock().expect("node_store lock poisoned");
            let aoi = self.aoi_nodes.lock().expect("aoi_nodes lock poisoned");
            aoi.iter()
                .filter_map(|id| {
                    store.nodes.get(id).map(|node| {
                        let p = node.position;
                        (id.clone(), crate::events::position_payload(p.x, p.y, p.z))
                    })
                })
                .collect()
        };
        for (id, payload) in positions {
            self.emit(EngineEvent::NodePositionUpdated(id, payload));
        }
        if self.connection_loss.update(connected_ids.len()) {
            let id = self.self_id.lock().expect("self_id lock poisoned").clone();
            self.emit(EngineEvent::AllConnectionsLost(id));
        }

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

        self.step_reorder_flush();
    }

    /// Delivers per-source reorder-buffer gaps that timed out without new
    /// traffic arriving to trigger `OverlayRouter::reorder_inbound`'s lazy
    /// flush (e.g. the sender went idle after a relay/direct route switch).
    /// Runs every tick so a stalled gap isn't held forever waiting for a
    /// message that will never arrive.
    fn step_reorder_flush(&self) {
        let Some(ov) = self.running_context().and_then(|ctx| ctx.overlay.clone()) else {
            return;
        };
        for (from, contents) in ov.flush_expired_inbound() {
            for content in contents {
                self.handle_message_content(from.clone(), content);
            }
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

    /// Notifies the overlay strategies that a peer disconnect has been
    /// confirmed, so a strategy can trigger an immediate re-selection
    /// instead of waiting for the next periodic `tick`. No-op if the engine
    /// isn't running. Transports call this right after a disconnect is
    /// confirmed.
    pub fn notify_peer_disconnected(&self) {
        let ctx = {
            let state = self.state.lock().expect("state lock poisoned");
            match &*state {
                EngineState::Running(ctx) => Some(ctx.clone()),
                _ => None,
            }
        };
        let Some(ctx) = ctx else { return };
        let Some(ov) = &ctx.overlay else { return };

        let states = ctx.active_connection_states();
        let config = self.config.lock().expect("config lock poisoned").clone();
        for action in ov.notify_peer_disconnected(&config, &states) {
            self.handle_action(action);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineEventHandler, RunningContext};
    use crate::events::*;
    use crate::overlay::{dnve3::Vector3, OverlayRouter};
    use crate::runtime::{AsyncRuntime, BoxTask};
    use crate::signaling::{MessageContent, SignalingHandler};
    use crate::transport::{NetworkEventHandler, Transport};
    use crate::types::DeliveryMethod;
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::sync::{Arc, Mutex};

    struct Runtime;
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    impl AsyncRuntime for Runtime {
        fn spawn(&self, _: BoxTask) {}
        async fn sleep(&self, _: web_time::Duration) {}
    }

    #[derive(Default)]
    struct SnapshotTransport(Mutex<Vec<NodeId>>);
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    impl Transport for SnapshotTransport {
        async fn start(&self, _: Arc<dyn NetworkEventHandler>) -> crate::Result<()> {
            Ok(())
        }
        async fn send(&self, _: &NodeId, _: Bytes, _: DeliveryMethod) -> crate::Result<()> {
            Ok(())
        }
        async fn broadcast(&self, _: Bytes, _: DeliveryMethod) -> crate::Result<()> {
            Ok(())
        }
        async fn connect(&self, _: &NodeId) -> crate::Result<()> {
            Ok(())
        }
        async fn disconnect(&self, _: &NodeId) -> crate::Result<()> {
            Ok(())
        }
        fn get_connection_state(&self, id: &NodeId) -> ConnectionState {
            if self.0.lock().unwrap().contains(id) {
                ConnectionState::Connected
            } else {
                ConnectionState::Disconnected
            }
        }
        fn get_connected_nodes(&self) -> Vec<NodeId> {
            self.0.lock().unwrap().clone()
        }
    }
    struct Signaling;
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    impl SignalingHandler for Signaling {
        async fn handle_message(&self, _: MessageContent) -> crate::Result<()> {
            Ok(())
        }
    }

    type CallbackEvent = (u32, NodeId, Vec<u8>);
    #[derive(Default)]
    struct Events(Mutex<Vec<CallbackEvent>>);
    impl EngineEventHandler for Events {
        fn on_event(&self, event: EngineEvent) {
            self.0.lock().unwrap().push(event.into_application_event());
        }
    }

    #[tokio::test]
    async fn tick_reports_aoi_positions_and_loss_once_per_connection_cycle() {
        let engine = MistEngine::new(Arc::new(Runtime));
        let other_room = MistEngine::new(Arc::new(Runtime));
        let events = Arc::new(Events::default());
        engine.set_event_handler(events.clone());
        let config = engine.config.lock().unwrap().clone();
        let local = NodeId("local".into());
        let peer = NodeId("peer".into());
        {
            let mut store = engine.node_store.lock().unwrap();
            store.update_node_position(local.clone(), Vector3::zero());
            store.update_node_position(peer.clone(), Vector3::new(1.0, 2.0, 3.0));
        }
        let transport = Arc::new(SnapshotTransport::default());
        *engine.state.lock().unwrap() = EngineState::Running(Arc::new(RunningContext {
            transport: transport.clone(),
            network_transport: None,
            signaling_handler: Arc::new(Signaling),
            p2p_signaling_handler: None,
            signaling_dispatch: None,
            websocket_signaler: None,
            overlay: Some(Arc::new(OverlayRouter::new(
                &config,
                engine.node_store.clone(),
                local.clone(),
            ))),
        }));
        engine.tick().await;
        assert!(!events
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.0 == EVENT_ALL_CONNECTIONS_LOST));
        for _ in 0..2 {
            *transport.0.lock().unwrap() = vec![peer.clone()];
            engine.tick().await;
            transport.0.lock().unwrap().clear();
            engine.tick().await;
            engine.tick().await;
        }
        assert!(!other_room.connection_loss.update(0));
        engine
            .node_store
            .lock()
            .unwrap()
            .update_node_position(peer.clone(), Vector3::new(100_000.0, 0.0, 0.0));
        engine.tick().await;
        let events = events.0.lock().unwrap();
        let left = events.iter().find(|e| e.0 == EVENT_AOI_LEFT).unwrap();
        assert_eq!((&left.1, left.2.as_slice()), (&peer, b"left".as_slice()));
        let entered = events.iter().find(|e| e.0 == EVENT_AOI_ENTERED).unwrap();
        assert_eq!(
            (&entered.1, entered.2.as_slice()),
            (&peer, b"entered".as_slice())
        );
        let positions: Vec<_> = events
            .iter()
            .filter(|e| e.0 == EVENT_NODE_POSITION_UPDATED)
            .collect();
        assert_eq!(
            positions.len(),
            7,
            "unchanged AOI positions are sent each tick"
        );
        for event in positions {
            assert_eq!(event.1, peer);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&event.2).unwrap(),
                serde_json::json!({"x":1,"y":2,"z":3})
            );
        }
        let losses: Vec<_> = events
            .iter()
            .filter(|e| e.0 == EVENT_ALL_CONNECTIONS_LOST)
            .collect();
        assert_eq!(losses.len(), 2);
        for event in losses {
            assert_eq!(event.1, local);
            assert_eq!(event.2, ALL_CONNECTIONS_LOST_PAYLOAD);
        }
    }
}
