use crate::types::NodeId;
use std::collections::{HashMap, HashSet};
use tracing::trace;
use web_time::{Duration, Instant};

const RECONNECT_COOLDOWN: Duration = Duration::from_secs(3);

pub struct RoutingTable {
    routes: HashMap<NodeId, NodeId>,
    route_expire_at: HashMap<NodeId, Instant>,
    reconnect_blocked_until: HashMap<NodeId, Instant>,
    pub connected_nodes: HashSet<NodeId>,
    pub message_nodes: HashSet<NodeId>,
    expire_seconds: u64,
}

impl RoutingTable {
    pub fn new(expire_seconds: u64) -> Self {
        Self {
            routes: HashMap::new(),
            route_expire_at: HashMap::new(),
            reconnect_blocked_until: HashMap::new(),
            connected_nodes: HashSet::new(),
            message_nodes: HashSet::new(),
            expire_seconds,
        }
    }

    pub fn add_routing(&mut self, source_id: NodeId, from_id: NodeId, self_id: &NodeId) {
        if &source_id == self_id {
            return;
        }
        if source_id == from_id {
            return;
        }

        if let Some(expire_at) = self.route_expire_at.get(&source_id) {
            if Instant::now() < *expire_at {
                return;
            }
        }

        trace!("[RoutingTable] Add {:?} via {:?}", source_id, from_id);

        self.routes.insert(source_id.clone(), from_id);
        self.route_expire_at.insert(
            source_id,
            Instant::now() + web_time::Duration::from_secs(self.expire_seconds),
        );
    }

    pub fn get_next_hop(&self, target: &NodeId) -> Option<NodeId> {
        if self.connected_nodes.contains(target) {
            return Some(target.clone());
        }

        let next_hop = self.routes.get(target).cloned();

        if let Some(ref node) = next_hop {
            trace!("[RoutingTable] Get {:?} -> {:?}", target, node);
        }

        next_hop
    }

    pub fn on_connected(&mut self, id: NodeId) {
        self.connected_nodes.insert(id.clone());
        self.reconnect_blocked_until.remove(&id);
    }

    pub fn block_reconnect(&mut self, id: NodeId) {
        self.reconnect_blocked_until
            .insert(id, Instant::now() + RECONNECT_COOLDOWN);
    }

    pub fn is_reconnect_blocked(&self, id: &NodeId) -> bool {
        matches!(self.reconnect_blocked_until.get(id), Some(until) if Instant::now() < *until)
    }

    pub fn on_disconnected(&mut self, id: &NodeId) {
        self.connected_nodes.remove(id);
        self.message_nodes.remove(id);
        self.block_reconnect(id.clone());

        self.remove_node_routes(id);

        let mut removed = Vec::new();
        for (source, next_hop) in &self.routes {
            if next_hop == id {
                removed.push(source.clone());
            }
        }
        for source in removed {
            self.remove_node_routes(&source);
        }
    }

    pub fn add_message_node(&mut self, id: NodeId) {
        self.message_nodes.insert(id);
    }

    pub fn remove_message_node(&mut self, id: &NodeId) {
        self.message_nodes.remove(id);
    }

    pub fn remove_node_routes(&mut self, id: &NodeId) {
        self.routes.remove(id);
        self.route_expire_at.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::RoutingTable;
    use crate::types::NodeId;

    fn node(id: &str) -> NodeId {
        NodeId(id.to_string())
    }

    #[test]
    fn add_routing_ignores_updates_before_expiry() {
        let self_id = node("self");
        let target = node("target");
        let first = node("relay-a");
        let second = node("relay-b");

        let mut rt = RoutingTable::new(60);
        rt.add_routing(target.clone(), first.clone(), &self_id);
        assert_eq!(rt.get_next_hop(&target), Some(first.clone()));

        rt.add_routing(target.clone(), second.clone(), &self_id);
        assert_eq!(rt.get_next_hop(&target), Some(first));
    }

    #[test]
    fn on_disconnected_removes_route_using_disconnected_node() {
        let self_id = node("self");
        let target = node("target");
        let relay = node("relay");

        let mut rt = RoutingTable::new(60);
        rt.on_connected(relay.clone());
        rt.add_routing(target.clone(), relay.clone(), &self_id);
        rt.on_disconnected(&relay);

        assert_eq!(rt.get_next_hop(&target), None);
    }

    #[test]
    fn on_disconnected_blocks_immediate_reconnect() {
        let relay = node("relay");
        let mut rt = RoutingTable::new(60);

        rt.on_disconnected(&relay);
        assert!(rt.is_reconnect_blocked(&relay));

        rt.on_connected(relay.clone());
        assert!(!rt.is_reconnect_blocked(&relay));
    }
}
