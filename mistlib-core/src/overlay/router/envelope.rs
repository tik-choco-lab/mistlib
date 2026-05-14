use super::OverlayRouter;
use crate::action::OverlayAction;
use crate::overlay::OverlayEnvelope;
use crate::signaling::MessageContent;
use crate::types::{DeliveryMethod, NodeId};
use bytes::Bytes;

enum ForwardTargets {
    Broadcast(Vec<NodeId>),
    Unicast(NodeId),
    None,
}

impl ForwardTargets {
    fn has_targets(&self) -> bool {
        !matches!(self, Self::None)
    }
}

impl OverlayRouter {
    pub fn handle_envelope(&self, envelope: OverlayEnvelope, from: NodeId) -> Vec<OverlayAction> {
        let is_broadcast = envelope.to.is_broadcast();
        let mut actions = self.deliver_to_strategies(&envelope, is_broadcast);

        if Self::can_forward(&envelope) {
            let targets = self.forward_targets(&envelope, &from, is_broadcast);
            actions.extend(self.forward_actions(envelope, targets));
        }

        actions
    }

    pub fn learn_route(&self, source: &NodeId, from: &NodeId) {
        if !self.should_learn_route(source, from) {
            return;
        }

        let mut rt = self
            .routing_table
            .lock()
            .expect("routing_table lock poisoned");
        rt.add_route(source.clone(), from.clone());
    }

    fn can_forward(envelope: &OverlayEnvelope) -> bool {
        envelope.hop_count > 0
    }

    fn should_learn_route(&self, source: &NodeId, from: &NodeId) -> bool {
        *source != self.local_node_id && *source != *from
    }

    fn should_deliver_to_strategies(&self, envelope: &OverlayEnvelope, is_broadcast: bool) -> bool {
        is_broadcast || envelope.to == self.local_node_id
    }

    fn is_local_destination(&self, envelope: &OverlayEnvelope) -> bool {
        envelope.to == self.local_node_id
    }

    /// Resolves the next forwarding target with a single routing table lock.
    fn forward_targets(
        &self,
        envelope: &OverlayEnvelope,
        from: &NodeId,
        is_broadcast: bool,
    ) -> ForwardTargets {
        let rt = self
            .routing_table
            .lock()
            .expect("routing_table lock poisoned");

        if is_broadcast {
            return ForwardTargets::Broadcast(
                rt.connected_nodes
                    .iter()
                    .filter(|id| **id != *from && **id != self.local_node_id)
                    .cloned()
                    .collect(),
            );
        }

        if self.is_local_destination(envelope) {
            return ForwardTargets::None;
        }

        ForwardTargets::Unicast(
            rt.get_next_hop(&envelope.to)
                .unwrap_or_else(|| envelope.to.clone()),
        )
    }

    /// Passes overlay messages to strategies when the envelope is local or broadcast.
    fn deliver_to_strategies(
        &self,
        envelope: &OverlayEnvelope,
        is_broadcast: bool,
    ) -> Vec<OverlayAction> {
        if !self.should_deliver_to_strategies(envelope, is_broadcast) {
            return Vec::new();
        }

        let MessageContent::Overlay(overlay_msg) = &envelope.content else {
            return Vec::new();
        };

        self.handle_overlay_message(
            envelope.from.clone(),
            overlay_msg.message_type,
            &overlay_msg.payload,
        )
    }

    /// Decrements the hop count and creates actions for the resolved forward targets.
    fn forward_actions(
        &self,
        mut envelope: OverlayEnvelope,
        targets: ForwardTargets,
    ) -> Vec<OverlayAction> {
        if !targets.has_targets() {
            return Vec::new();
        }

        envelope.hop_count -= 1;
        let Ok(serialized) = bincode::serialize(&envelope) else {
            return Vec::new();
        };
        let data = Bytes::from(serialized);

        match targets {
            ForwardTargets::Broadcast(neighbors) => neighbors
                .into_iter()
                .map(|to| OverlayAction::SendMessage {
                    to,
                    data: data.clone(),
                    method: DeliveryMethod::ReliableOrdered,
                })
                .collect(),
            ForwardTargets::Unicast(next_hop) => vec![OverlayAction::SendMessage {
                to: next_hop,
                data,
                method: DeliveryMethod::ReliableOrdered,
            }],
            ForwardTargets::None => Vec::new(),
        }
    }
}
