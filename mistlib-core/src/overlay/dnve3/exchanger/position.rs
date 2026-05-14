use super::DNVE3Exchanger;
use crate::overlay::dnve3::spatial_density::Vector3;
use crate::types::NodeId;

impl DNVE3Exchanger {
    pub fn handle_position(&self, from: NodeId, payload: &[u8]) {
        if let Ok(pos) = bincode::deserialize::<Vector3>(payload) {
            self.node_store
                .lock()
                .unwrap()
                .update_node_position(from, pos);
        }
    }
}
