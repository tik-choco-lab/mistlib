use crate::action::OverlayAction;
use crate::overlay::{ActionHandler, OverlayOptimizer};
use crate::signaling::{MessageContent, Signaler, SignalingEnvelope};
use crate::transport::{NetworkEventHandler, Transport};
use crate::types::{ConnectionState, DeliveryMethod, NodeId};
use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;

pub struct OverlayTransport {
    pub optimizer: Arc<OverlayOptimizer>,
    pub action_handler: Arc<dyn ActionHandler>,
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Transport for OverlayTransport {
    async fn start(&self, _handler: Arc<dyn NetworkEventHandler>) -> crate::error::Result<()> {
        Ok(())
    }

    async fn send(
        &self,
        to: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> crate::error::Result<()> {
        let action = self.optimizer.wrap_data(to, data, method);
        self.action_handler.handle_action(action);
        Ok(())
    }

    async fn broadcast(&self, data: Bytes, method: DeliveryMethod) -> crate::error::Result<()> {
        let action = self
            .optimizer
            .wrap_data(&NodeId("".to_string()), data, method);
        self.action_handler.handle_action(action);
        Ok(())
    }

    fn get_connection_state(&self, _node: &NodeId) -> ConnectionState {
        ConnectionState::Connected
    }

    async fn connect(&self, _node: &NodeId) -> crate::error::Result<()> {
        Ok(())
    }

    async fn disconnect(&self, _node: &NodeId) -> crate::error::Result<()> {
        Ok(())
    }

    fn get_connected_nodes(&self) -> Vec<NodeId> {
        let rt = self.optimizer.routing_table.lock().unwrap();
        rt.connected_nodes.iter().cloned().collect()
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Signaler for OverlayTransport {
    async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> crate::error::Result<()> {
        let envelope = SignalingEnvelope {
            from: self.optimizer.local_node_id.clone(),
            to: to.clone(),
            hop_count: self.optimizer.hop_count,
            content: msg,
        };
        let data = bincode::serialize(&envelope)
            .map_err(|e| crate::error::MistError::Internal(e.to_string()))?;
        let next_hop = {
            let rt = self.optimizer.routing_table.lock().unwrap();
            rt.get_next_hop(to).unwrap_or_else(|| to.clone())
        };

        self.action_handler
            .handle_action(OverlayAction::SendMessage {
                to: next_hop,
                data: Bytes::from(data),
                method: DeliveryMethod::ReliableOrdered,
            });
        Ok(())
    }
}
