#[path = "../../src/transport/webrtc/ice_state.rs"]
mod ice_state;
#[path = "../../src/transport/webrtc/recovery.rs"]
mod recovery;

use async_trait::async_trait;
use bytes::Bytes;
use mistlib_core::config::Config;
use mistlib_core::engine::{EngineState, MistEngine, RunningContext};
use mistlib_core::overlay::OverlayRouter;
use mistlib_core::runtime::{AsyncRuntime, BoxTask};
use mistlib_core::signaling::{MessageContent, SignalingHandler};
use mistlib_core::transport::{NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use mistlib_core::Result;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use web_sys::RtcIceConnectionState;

struct NoTasks;
#[async_trait]
impl AsyncRuntime for NoTasks {
    fn spawn(&self, _: BoxTask) {}
    async fn sleep(&self, _: web_time::Duration) {}
}

struct Network {
    states: RwLock<HashMap<NodeId, ConnectionState>>,
    reads: AtomicUsize,
}
#[async_trait]
impl Transport for Network {
    async fn start(&self, _: Arc<dyn NetworkEventHandler>) -> Result<()> {
        Ok(())
    }
    async fn send(&self, _: &NodeId, _: Bytes, _: DeliveryMethod) -> Result<()> {
        Ok(())
    }
    async fn broadcast(&self, _: Bytes, _: DeliveryMethod) -> Result<()> {
        Ok(())
    }
    fn get_connection_state(&self, node: &NodeId) -> ConnectionState {
        self.states.read().unwrap()[node]
    }
    async fn connect(&self, _: &NodeId) -> Result<()> {
        Ok(())
    }
    async fn disconnect(&self, _: &NodeId) -> Result<()> {
        Ok(())
    }
    fn get_connected_nodes(&self) -> Vec<NodeId> {
        // Fail immediately instead of hanging the host test on a regression.
        let states = self
            .states
            .try_read()
            .expect("callback retained state write guard");
        self.reads.fetch_add(1, Ordering::SeqCst);
        states
            .iter()
            .filter(|(_, state)| **state == ConnectionState::Connected)
            .map(|(node, _)| node.clone())
            .collect()
    }
}
#[async_trait]
impl SignalingHandler for Network {
    async fn handle_message(&self, _: MessageContent) -> Result<()> {
        Ok(())
    }
}

#[test]
fn failed_and_closed_release_state_before_cleanup_and_engine_notification() {
    for ice in [RtcIceConnectionState::Failed, RtcIceConnectionState::Closed] {
        let remote = NodeId("remote".into());
        let network = Arc::new(Network {
            states: RwLock::new(HashMap::from([(
                remote.clone(),
                ConnectionState::Connected,
            )])),
            reads: AtomicUsize::new(0),
        });
        let engine = MistEngine::new(Arc::new(NoTasks));
        let router = Arc::new(OverlayRouter::new(
            &Config::new_default(),
            engine.node_store.clone(),
            NodeId("local".into()),
        ));
        // Production chooses network_transport for the immediate disconnect
        // re-selection, even when the primary transport is an overlay.
        *engine.state.lock().unwrap() = EngineState::Running(Arc::new(RunningContext {
            transport: network.clone(),
            network_transport: Some(network.clone()),
            signaling_handler: network.clone(),
            p2p_signaling_handler: None,
            signaling_dispatch: None,
            websocket_signaler: None,
            overlay: Some(router),
        }));
        let mut emitted = false;
        ice_state::handle_ice_state_change(&network.states, &remote, ice, false, |state| {
            assert_eq!(state, Some(ConnectionState::Disconnected));
            // Both cleanup (including a channel close) and app notification
            // may synchronously re-enter the transport after the mutation.
            assert_eq!(
                network.states.try_write().unwrap()[&remote],
                ConnectionState::Disconnected
            );
            assert!(network.get_connected_nodes().is_empty());
            engine.notify_peer_disconnected();
            emitted = true;
        });
        assert!(emitted);
        assert_eq!(network.reads.load(Ordering::SeqCst), 2, "{ice:?}");
    }
}
