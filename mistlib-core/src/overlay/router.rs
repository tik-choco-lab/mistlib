use crate::config::Config;
use crate::overlay::node_store::NodeStore;
use crate::overlay::routing_table::RoutingTable;
use crate::overlay::TopologyStrategy;
use crate::types::{ConnectionState, NodeId};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

mod envelope;
mod send;
mod strategies;

pub struct OverlayRouter {
    pub node_store: Arc<Mutex<NodeStore>>,
    pub routing_table: Arc<Mutex<RoutingTable>>,
    pub strategies: Vec<Arc<dyn TopologyStrategy>>,
    pub local_node_id: NodeId,
    pub hop_count: u32,
}

impl OverlayRouter {
    pub fn new(config: &Config, node_store: Arc<Mutex<NodeStore>>, local_node_id: NodeId) -> Self {
        let routing_table = Arc::new(Mutex::new(RoutingTable::new()));

        Self {
            node_store,
            routing_table,
            strategies: Vec::new(),
            local_node_id,
            hop_count: config.limits.hop_count,
        }
    }

    /// Synchronises the routing table's direct connected set with a transport snapshot.
    pub fn sync_connection_states(
        &self,
        connected_node_states: &[(NodeId, ConnectionState)],
    ) -> HashSet<NodeId> {
        let connected = connected_node_states
            .iter()
            .filter(|(_, state)| *state == ConnectionState::Connected)
            .map(|(id, _)| id.clone())
            .collect::<HashSet<_>>();
        self.sync_connected_nodes(&connected);
        connected
    }

    pub fn sync_connected_nodes(&self, connected: &HashSet<NodeId>) {
        let mut rt = self
            .routing_table
            .lock()
            .expect("routing_table lock poisoned");
        let previous = rt.connected_nodes.clone();
        for id in connected {
            rt.on_connected(id.clone());
        }
        for id in previous {
            if !connected.contains(&id) {
                rt.on_disconnected(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> OverlayRouter {
        OverlayRouter::new(
            &Config::new_default(),
            Arc::new(Mutex::new(NodeStore::new())),
            NodeId("local".to_string()),
        )
    }

    #[test]
    fn sync_connection_states_keeps_only_connected_nodes() {
        let router = router();
        let connected = router.sync_connection_states(&[
            (NodeId("connected".to_string()), ConnectionState::Connected),
            (
                NodeId("connecting".to_string()),
                ConnectionState::Connecting,
            ),
            (
                NodeId("disconnected".to_string()),
                ConnectionState::Disconnected,
            ),
        ]);

        assert!(connected.contains(&NodeId("connected".to_string())));
        assert!(!connected.contains(&NodeId("connecting".to_string())));

        let rt = router.routing_table.lock().unwrap();
        assert!(rt
            .connected_nodes
            .contains(&NodeId("connected".to_string())));
        assert!(!rt
            .connected_nodes
            .contains(&NodeId("connecting".to_string())));
    }

    #[test]
    fn sync_connection_states_removes_routes_via_disconnected_nodes() {
        let router = router();
        let relay = NodeId("relay".to_string());
        let target = NodeId("target".to_string());
        router.sync_connection_states(&[(relay.clone(), ConnectionState::Connected)]);
        router
            .routing_table
            .lock()
            .unwrap()
            .add_route(target.clone(), relay.clone());

        router.sync_connection_states(&[]);

        assert_eq!(
            router.routing_table.lock().unwrap().get_next_hop(&target),
            None
        );
    }
}
