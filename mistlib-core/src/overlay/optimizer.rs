use crate::action::OverlayAction;
use crate::config::Config;
use crate::overlay::node_store::NodeStore;
use crate::overlay::routing_table::RoutingTable;
use crate::overlay::TopologyStrategy;
use crate::signaling::{MessageContent, SignalingEnvelope};
use crate::types::{DeliveryMethod, NodeId};
use bytes::Bytes;
use std::sync::{Arc, Mutex};

pub struct OverlayOptimizer {
    pub routing_table: Arc<Mutex<RoutingTable>>,
    pub node_store: Arc<Mutex<NodeStore>>,
    pub strategies: Vec<Arc<dyn TopologyStrategy>>,
    pub local_node_id: NodeId,
    pub hop_count: u32,
}

impl OverlayOptimizer {
    pub fn new(config: &Config, node_store: Arc<Mutex<NodeStore>>, local_node_id: NodeId) -> Self {
        let routing_table = Arc::new(Mutex::new(RoutingTable::new(
            config.limits.expire_node_seconds as u64,
        )));

        Self {
            routing_table,
            node_store,
            strategies: Vec::new(),
            local_node_id,
            hop_count: config.limits.hop_count,
        }
    }

    pub fn add_strategy(&mut self, strategy: Arc<dyn TopologyStrategy>) {
        self.strategies.push(strategy);
    }

    pub async fn start(
        &self,
        runtime: Arc<dyn crate::runtime::AsyncRuntime>,
        config: Arc<Config>,
        action_handler: Arc<dyn crate::overlay::ActionHandler>,
    ) {
        for strategy in &self.strategies {
            strategy
                .start(runtime.clone(), config.clone(), action_handler.clone())
                .await;
        }
    }

    pub fn handle_overlay_message(
        &self,
        from: NodeId,
        message_type: u32,
        payload: &[u8],
    ) -> Vec<OverlayAction> {
        let mut actions = Vec::new();
        for strategy in &self.strategies {
            actions.extend(strategy.handle_message(&from, message_type, payload));
        }
        actions
    }

    pub fn handle_envelope(&self, envelope: SignalingEnvelope, from: NodeId) -> Vec<OverlayAction> {
        let mut actions = Vec::new();
        {
            let mut rt = self.routing_table.lock().unwrap();
            rt.add_routing(envelope.from.clone(), from.clone(), &self.local_node_id);
        }

        let is_broadcast = envelope.to.0.is_empty();
        if envelope.to == self.local_node_id || is_broadcast {
            if let MessageContent::Overlay(overlay_msg) = &envelope.content {
                actions.extend(self.handle_overlay_message(
                    envelope.from.clone(),
                    overlay_msg.message_type,
                    &overlay_msg.payload,
                ));
            }
        }

        if is_broadcast && envelope.hop_count > 0 {
            let mut forwarded = envelope.clone();
            forwarded.hop_count -= 1;
            if let Ok(serialized) = bincode::serialize(&forwarded) {
                let data = Bytes::from(serialized);
                let neighbors: Vec<NodeId> = {
                    let rt = self.routing_table.lock().unwrap();
                    rt.connected_nodes
                        .iter()
                        .filter(|id| **id != from && **id != self.local_node_id)
                        .cloned()
                        .collect()
                };

                for neighbor in neighbors {
                    actions.push(OverlayAction::SendMessage {
                        to: neighbor,
                        data: data.clone(),
                        method: DeliveryMethod::ReliableOrdered,
                    });
                }
            }
        }

        if !is_broadcast && envelope.to != self.local_node_id && envelope.hop_count > 0 {
            let mut forwarded = envelope.clone();
            forwarded.hop_count -= 1;
            if let Ok(serialized) = bincode::serialize(&forwarded) {
                actions.push(self.create_send_action(
                    &envelope.to,
                    Bytes::from(serialized),
                    DeliveryMethod::ReliableOrdered,
                ));
            }
        }
        actions
    }

    fn create_send_action(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> OverlayAction {
        let next_hop = {
            let rt = self.routing_table.lock().unwrap();
            rt.get_next_hop(node).unwrap_or_else(|| node.clone())
        };

        OverlayAction::SendMessage {
            to: next_hop,
            data,
            method,
        }
    }

    pub fn wrap_data(&self, to: &NodeId, data: Bytes, method: DeliveryMethod) -> OverlayAction {
        let envelope = SignalingEnvelope {
            from: self.local_node_id.clone(),
            to: to.clone(),
            hop_count: self.hop_count,
            content: MessageContent::Raw(data.clone()),
        };
        let enveloped_data = bincode::serialize(&envelope).unwrap_or_default();
        self.create_send_action(to, Bytes::from(enveloped_data), method)
    }

    pub fn tick(
        &self,
        config: &Config,
        connected_node_states: &[(NodeId, crate::types::ConnectionState)],
    ) -> Vec<OverlayAction> {
        let mut actions = Vec::new();
        for strategy in &self.strategies {
            actions.extend(strategy.tick(config, connected_node_states));
        }
        actions
    }
}
