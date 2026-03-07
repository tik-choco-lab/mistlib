use async_trait::async_trait;
use bytes::Bytes;
use mistlib_core::error::Result;
use mistlib_core::layers::{L1Notifier, L1Transport};
use mistlib_core::overlay::dnve3::Vector3;
use mistlib_core::overlay::node_store::NodeStore;
use mistlib_core::transport::Transport;
use mistlib_core::types::{DeliveryMethod, NodeId};
use std::sync::{Arc, Mutex};
use tracing::debug;

pub struct NativeL1Transport {
    transport: Arc<dyn Transport>,
    node_store: Arc<Mutex<NodeStore>>,
    local_id: NodeId,
}

impl NativeL1Transport {
    pub fn new(
        transport: Arc<dyn Transport>,
        node_store: Arc<Mutex<NodeStore>>,
        local_id: NodeId,
    ) -> Self {
        Self {
            transport,
            node_store,
            local_id,
        }
    }
}

#[async_trait]
impl L1Transport for NativeL1Transport {
    fn update_position(&self, x: f32, y: f32, z: f32) {
        debug!("L1: Updating position ({}, {}, {})", x, y, z);
        let mut store = self.node_store.lock().unwrap();
        store.update_node_position(self.local_id.clone(), Vector3::new(x, y, z));
    }
    async fn send_message(
        &self,
        target_id: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> Result<()> {
        self.transport.send(target_id, data, method).await
    }

    async fn broadcast(&self, data: Bytes, method: DeliveryMethod) -> Result<()> {
        self.transport.broadcast(data, method).await
    }
}

impl L1Notifier for NativeL1Transport {
    fn notify_connected(&self, node_id: &NodeId) {
        crate::events::on_connected_internal(node_id.clone());
    }

    fn notify_disconnected(&self, node_id: &NodeId) {
        crate::events::on_disconnected_internal(node_id.clone());
    }

    fn notify_node_position_updated(&self, node_id: &NodeId, x: f32, y: f32, z: f32) {
        let payload = format!("{{\"x\":{}, \"y\":{}, \"z\":{}}}", x, y, z);
        crate::events::dispatch_event(
            crate::events::EVENT_NODE_POSITION_UPDATED,
            node_id,
            payload.as_bytes(),
        );
    }
}
