use super::{DNVE3Exchanger, MAX_NODE_LIST_NODES};
use crate::overlay::node_store::NodeInfo as NodeStoreNode;
use crate::types::NodeId;

impl DNVE3Exchanger {
    pub(super) fn apply_node_list(&self, from: &NodeId, nodes: Vec<NodeStoreNode>) {
        let mut node_store = self.node_store.lock().unwrap();
        let mut dnve_data_store = self.dnve_data_store.lock().unwrap();
        let mut routing_table = self.routing_table.lock().unwrap();

        for node in nodes.into_iter().take(MAX_NODE_LIST_NODES) {
            if node.id == self.local_node_id || node.id.is_broadcast() {
                continue;
            }
            dnve_data_store.update_last_message_time(&node.id);
            routing_table.add_route(node.id.clone(), from.clone());
            node_store.update_node_position(node.id, node.position);
        }

        dnve_data_store.aoi_nodes =
            node_store.get_nodes_in_range(&self.local_node_id, self.aoi_range);
    }
}
