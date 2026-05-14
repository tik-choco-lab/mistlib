use super::{DNVE3Exchanger, MIN_NODE_LIST_AOI_DISTANCE};
use crate::config::ConnectionMode;
use crate::overlay::dnve3::spatial_density::Vector3;
use crate::overlay::node_store::{NodeInfo as NodeStoreNode, NodeStore};
use crate::overlay::routing_table::RoutingTable;
use crate::types::NodeId;
use std::collections::HashSet;

impl DNVE3Exchanger {
    pub(super) fn node_list_for_request(
        &self,
        requester: &NodeId,
        mode: ConnectionMode,
    ) -> Vec<NodeStoreNode> {
        let node_store = self.node_store.lock().unwrap();
        let routing_table = self.routing_table.lock().unwrap();

        match mode {
            ConnectionMode::DirectionDensity => {
                self.direction_density_node_list(&node_store, &routing_table, requester)
            }
            ConnectionMode::NodeListAoiGuard
            | ConnectionMode::NodeListAoiProximity
            | ConnectionMode::NodeListAoiDensity
            | ConnectionMode::PSense => {
                self.node_list_with_self_and_requester_aoi(&node_store, &routing_table, requester)
            }
            ConnectionMode::DirectionDensityLight
            | ConnectionMode::NodeListDirectional
            | ConnectionMode::NodeListProximity => {
                self.node_list_with_self(&node_store, &routing_table)
            }
        }
    }

    fn connected_node_list(
        node_store: &NodeStore,
        routing_table: &RoutingTable,
    ) -> Vec<NodeStoreNode> {
        let mut nodes = node_store
            .nodes
            .values()
            .filter(|n| routing_table.connected_nodes.contains(&n.id))
            .cloned()
            .collect::<Vec<_>>();
        nodes.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        nodes
    }

    fn node_list_with_self(
        &self,
        node_store: &NodeStore,
        routing_table: &RoutingTable,
    ) -> Vec<NodeStoreNode> {
        let mut nodes = Vec::with_capacity(routing_table.connected_nodes.len() + 1);
        nodes.push(
            node_store
                .nodes
                .get(&self.local_node_id)
                .cloned()
                .unwrap_or_else(|| NodeStoreNode {
                    id: self.local_node_id.clone(),
                    position: Vector3::zero(),
                }),
        );
        nodes.extend(
            Self::connected_node_list(node_store, routing_table)
                .into_iter()
                .filter(|n| n.id != self.local_node_id),
        );
        nodes
    }

    fn node_list_with_self_and_requester_aoi(
        &self,
        node_store: &NodeStore,
        routing_table: &RoutingTable,
        requester: &NodeId,
    ) -> Vec<NodeStoreNode> {
        let mut nodes = self.node_list_with_self(node_store, routing_table);
        self.extend_requester_aoi_nodes(node_store, requester, &mut nodes);
        nodes
    }

    fn direction_density_node_list(
        &self,
        node_store: &NodeStore,
        routing_table: &RoutingTable,
        requester: &NodeId,
    ) -> Vec<NodeStoreNode> {
        let mut nodes = Self::connected_node_list(node_store, routing_table);
        self.extend_requester_aoi_nodes(node_store, requester, &mut nodes);
        nodes
    }

    fn extend_requester_aoi_nodes(
        &self,
        node_store: &NodeStore,
        requester: &NodeId,
        nodes: &mut Vec<NodeStoreNode>,
    ) {
        let mut seen = nodes.iter().map(|n| n.id.clone()).collect::<HashSet<_>>();

        let Some(requester_pos) = node_store.nodes.get(requester).map(|n| n.position) else {
            return;
        };

        let mut candidates = node_store
            .nodes
            .values()
            .filter(|n| n.id != self.local_node_id && &n.id != requester && !seen.contains(&n.id))
            .filter_map(|n| {
                let dist = n.position.dist(requester_pos);
                if !(MIN_NODE_LIST_AOI_DISTANCE..=self.aoi_range).contains(&dist) {
                    return None;
                }
                Some((dist, n.clone()))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.id.0.cmp(&b.1.id.0)));

        for (_, node) in candidates.into_iter().take(self.node_list_aoi_extra_limit) {
            if seen.insert(node.id.clone()) {
                nodes.push(node);
            }
        }
    }
}
