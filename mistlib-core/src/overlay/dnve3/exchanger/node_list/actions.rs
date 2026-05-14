use super::{DNVE3Exchanger, MAX_NODE_LIST_NODES};
use crate::action::OverlayAction;
use crate::overlay::node_store::NodeInfo as NodeStoreNode;
use crate::overlay::{
    OverlayEnvelope, OverlayMessage, OVERLAY_MSG_NODE_LIST, OVERLAY_MSG_REQUEST_NODE_LIST,
};
use crate::signaling::MessageContent;
use crate::types::{DeliveryMethod, NodeId};

impl DNVE3Exchanger {
    pub(super) fn request_node_list_action(&self, to: &NodeId) -> Option<OverlayAction> {
        let payload = self
            .node_store
            .lock()
            .unwrap()
            .nodes
            .get(&self.local_node_id)
            .and_then(|node| bincode::serialize(&node.position).ok())
            .unwrap_or_default();

        let envelope = OverlayEnvelope {
            from: self.local_node_id.clone(),
            to: to.clone(),
            hop_count: self.hop_count,
            content: MessageContent::Overlay(OverlayMessage {
                message_type: OVERLAY_MSG_REQUEST_NODE_LIST,
                payload,
            }),
        };

        let data = bincode::serialize(&envelope)
            .map_err(|e| {
                tracing::warn!(
                    "[DNVE3] failed to serialize request_node_list to {}: {}",
                    to,
                    e
                )
            })
            .ok()?;
        Some(OverlayAction::SendMessage {
            to: to.clone(),
            data: bytes::Bytes::from(data),
            method: DeliveryMethod::ReliableOrdered,
        })
    }

    pub(super) fn node_list_response_action(
        &self,
        to: NodeId,
        nodes: &[NodeStoreNode],
    ) -> Option<OverlayAction> {
        let limited_len = nodes.len().min(MAX_NODE_LIST_NODES);
        let payload = match bincode::serialize(&nodes[..limited_len]) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    "[DNVE3] failed to serialize node_list payload to {}: {}",
                    to,
                    e,
                );
                return None;
            }
        };
        let envelope = OverlayEnvelope {
            from: self.local_node_id.clone(),
            to: to.clone(),
            hop_count: self.hop_count,
            content: MessageContent::Overlay(OverlayMessage {
                message_type: OVERLAY_MSG_NODE_LIST,
                payload,
            }),
        };

        let data = bincode::serialize(&envelope)
            .map_err(|e| {
                tracing::warn!(
                    "[DNVE3] failed to serialize node_list envelope to {}: {}",
                    to,
                    e
                )
            })
            .ok()?;
        Some(OverlayAction::SendMessage {
            to,
            data: bytes::Bytes::from(data),
            method: DeliveryMethod::ReliableOrdered,
        })
    }
}
