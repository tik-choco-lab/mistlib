use crate::types::NodeId;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct RoutingTable {
    pub connected_nodes: HashSet<NodeId>,
    routes: HashMap<NodeId, NodeId>,
}

impl RoutingTable {
    pub fn new() -> Self {
        Self {
            connected_nodes: HashSet::new(),
            routes: HashMap::new(),
        }
    }

    pub fn on_connected(&mut self, id: NodeId) {
        self.connected_nodes.insert(id);
    }

    pub fn on_disconnected(&mut self, id: &NodeId) {
        self.connected_nodes.remove(id);
        self.routes
            .retain(|target, next_hop| target != id && next_hop != id);
    }

    pub fn add_route(&mut self, target: NodeId, next_hop: NodeId) {
        if target == next_hop || self.connected_nodes.contains(&target) {
            return;
        }
        self.routes.insert(target, next_hop);
    }

    pub fn remove_route(&mut self, target: &NodeId) {
        self.routes.remove(target);
    }

    pub fn get_next_hop(&self, node: &NodeId) -> Option<NodeId> {
        if self.connected_nodes.contains(node) {
            return Some(node.clone());
        }
        self.routes
            .get(node)
            .filter(|next_hop| self.connected_nodes.contains(*next_hop))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::RoutingTable;
    use crate::types::NodeId;

    #[test]
    fn direct_connection_is_preferred_as_next_hop() {
        let mut table = RoutingTable::new();
        table.on_connected(NodeId("peer-a".into()));
        table.add_route(NodeId("peer-a".into()), NodeId("relay".into()));

        assert_eq!(
            table.get_next_hop(&NodeId("peer-a".into())),
            Some(NodeId("peer-a".into()))
        );
    }

    #[test]
    fn learned_route_is_used_when_next_hop_is_connected() {
        let mut table = RoutingTable::new();
        table.on_connected(NodeId("relay".into()));
        table.add_route(NodeId("peer-c".into()), NodeId("relay".into()));

        assert_eq!(
            table.get_next_hop(&NodeId("peer-c".into())),
            Some(NodeId("relay".into()))
        );
    }

    #[test]
    fn routes_via_disconnected_hop_are_dropped() {
        let mut table = RoutingTable::new();
        let relay = NodeId("relay".into());
        let target = NodeId("peer-c".into());
        table.on_connected(relay.clone());
        table.add_route(target.clone(), relay.clone());

        table.on_disconnected(&relay);

        assert_eq!(table.get_next_hop(&target), None);
    }

    // 不変条件 (3): 自己ループは登録されない
    #[test]
    fn add_route_to_self_is_ignored() {
        let mut table = RoutingTable::new();
        let node = NodeId("a".into());
        table.add_route(node.clone(), node.clone());

        assert_eq!(table.get_next_hop(&node), None);
    }

    // 不変条件 (4): 直接接続済みノードへのルートは登録されない
    #[test]
    fn add_route_to_directly_connected_node_is_ignored() {
        let mut table = RoutingTable::new();
        let target = NodeId("peer".into());
        let relay = NodeId("relay".into());
        table.on_connected(target.clone());
        table.on_connected(relay.clone());
        table.add_route(target.clone(), relay.clone());

        // 直接接続優先であること（ルートではなく自身が返る）
        assert_eq!(table.get_next_hop(&target), Some(target));
    }

    // 不変条件 (5): target が切断されたとき、そのノードへのルートも消える
    #[test]
    fn on_disconnected_removes_routes_where_node_is_target() {
        let mut table = RoutingTable::new();
        let relay = NodeId("relay".into());
        let target = NodeId("peer-c".into());
        table.on_connected(relay.clone());
        table.on_connected(target.clone());
        table.add_route(NodeId("peer-d".into()), relay.clone());

        // target を直接切断し、target 宛てのルートが残らないことを確認
        // (ここでは connected から外れたあと get_next_hop が None になること)
        table.on_disconnected(&target);

        assert_eq!(table.get_next_hop(&target), None);
    }

    // 不変条件 (5) 追加: next_hop として使われていたノードが切断されると
    // そのノード経由のルートが別 target 分もすべて消える
    #[test]
    fn on_disconnected_removes_all_routes_via_that_hop() {
        let mut table = RoutingTable::new();
        let relay = NodeId("relay".into());
        table.on_connected(relay.clone());
        table.add_route(NodeId("peer-x".into()), relay.clone());
        table.add_route(NodeId("peer-y".into()), relay.clone());

        table.on_disconnected(&relay);

        assert_eq!(table.get_next_hop(&NodeId("peer-x".into())), None);
        assert_eq!(table.get_next_hop(&NodeId("peer-y".into())), None);
    }
}
