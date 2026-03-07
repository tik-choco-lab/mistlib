use crate::types::NodeId;
use std::collections::{HashMap, HashSet};
use tracing::trace;

pub struct RoutingTable {
    routes: HashMap<NodeId, HashMap<NodeId, i32>>,
    pub connected_nodes: HashSet<NodeId>,
    pub message_nodes: HashSet<NodeId>,
}

impl RoutingTable {
    pub fn new(_expire_seconds: u64) -> Self {
        Self {
            routes: HashMap::new(),
            connected_nodes: HashSet::new(),
            message_nodes: HashSet::new(),
        }
    }

    pub fn add_routing(&mut self, source_id: NodeId, from_id: NodeId, self_id: &NodeId) {
        if &source_id == self_id {
            return;
        }
        if source_id == from_id {
            return;
        }

        trace!("[RoutingTable] Add {:?} via {:?}", source_id, from_id);

        let from_map = self
            .routes
            .entry(source_id.clone())
            .or_insert_with(HashMap::new);

        let score = from_map.entry(from_id.clone()).or_insert(0);
        *score += 1;

        let mut to_remove = Vec::new();
        for (node_id, score) in from_map.iter_mut() {
            if *node_id == from_id {
                continue;
            }
            *score -= 1;
            if *score <= 0 {
                to_remove.push(node_id.clone());
            }
        }

        for node_id in to_remove {
            from_map.remove(&node_id);
        }
    }

    pub fn get_next_hop(&self, target: &NodeId) -> Option<NodeId> {
        if self.connected_nodes.contains(target) {
            return Some(target.clone());
        }

        let from_map = self.routes.get(target)?;

        let mut best_node = None;
        let mut best_score = i32::MIN;

        for (node_id, &score) in from_map {
            if self.connected_nodes.contains(node_id) {
                if score > best_score {
                    best_score = score;
                    best_node = Some(node_id.clone());
                }
            }
        }

        if let Some(ref node) = best_node {
            trace!("[RoutingTable] Get {:?} -> {:?}", target, node);
        }

        best_node
    }

    pub fn on_connected(&mut self, id: NodeId) {
        self.connected_nodes.insert(id);
    }

    pub fn on_disconnected(&mut self, id: &NodeId) {
        self.connected_nodes.remove(id);
        self.message_nodes.remove(id);

        for from_map in self.routes.values_mut() {
            from_map.remove(id);
        }

        self.cleanup_routes();
    }

    pub fn add_message_node(&mut self, id: NodeId) {
        self.message_nodes.insert(id);
    }

    pub fn remove_message_node(&mut self, id: &NodeId) {
        self.message_nodes.remove(id);
    }

    pub fn cleanup_routes(&mut self) {
        self.routes.retain(|_, from_map| !from_map.is_empty());
    }

    pub fn remove_node_routes(&mut self, id: &NodeId) {
        self.routes.remove(id);
        for from_map in self.routes.values_mut() {
            from_map.remove(id);
        }
    }
}
