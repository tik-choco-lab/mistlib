mod actions;
mod apply;
mod selection;

use super::DNVE3Exchanger;
use crate::action::OverlayAction;
use crate::config::ConnectionMode;
use crate::overlay::node_store::NodeInfo as NodeStoreNode;
use crate::types::NodeId;

const MIN_NODE_LIST_AOI_DISTANCE: f32 = 0.001;
const MAX_NODE_LIST_NODES: usize = 128;

impl DNVE3Exchanger {
    pub fn send_request_node_list(&self, to: &NodeId) -> Vec<OverlayAction> {
        self.request_node_list_action(to).into_iter().collect()
    }

    pub fn send_node_list_push(&self, to: &NodeId, mode: ConnectionMode) -> Vec<OverlayAction> {
        let nodes = self.node_list_for_request(to, mode);
        self.node_list_response_action(to.clone(), &nodes)
            .into_iter()
            .collect()
    }

    pub fn handle_request_node_list(
        &self,
        from: NodeId,
        mode: ConnectionMode,
    ) -> Vec<OverlayAction> {
        self.handle_request_node_list_with_payload(from, mode, &[])
    }

    pub fn handle_request_node_list_with_payload(
        &self,
        from: NodeId,
        mode: ConnectionMode,
        payload: &[u8],
    ) -> Vec<OverlayAction> {
        if let Ok(position) =
            bincode::deserialize::<crate::overlay::dnve3::spatial_density::Vector3>(payload)
        {
            self.node_store
                .lock()
                .unwrap()
                .update_node_position(from.clone(), position);
        }

        let nodes = self.node_list_for_request(&from, mode);
        self.node_list_response_action(from, &nodes)
            .into_iter()
            .collect()
    }

    pub fn handle_node_list(&self, from: &NodeId, payload: &[u8]) {
        if let Ok(nodes) = bincode::deserialize::<Vec<NodeStoreNode>>(payload) {
            self.apply_node_list(from, nodes);
        }
    }
}
