use crate::overlay::dnve3::Vector3;
use crate::types::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use web_time::Instant;

#[derive(Serialize, Deserialize, Clone)]
pub struct NodeInfo {
    pub id: NodeId,
    pub position: Vector3,
}

pub struct NodeStore {
    pub nodes: HashMap<NodeId, NodeInfo>,
    pub last_updated: HashMap<NodeId, Instant>,
}

impl Default for NodeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeStore {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            last_updated: HashMap::new(),
        }
    }

    pub fn update_node_position(&mut self, id: NodeId, position: Vector3) {
        self.nodes
            .entry(id.clone())
            .and_modify(|n| n.position = position)
            .or_insert_with(|| NodeInfo {
                id: id.clone(),
                position,
            });
        self.last_updated.insert(id, Instant::now());
    }

    pub fn touch_node(&mut self, id: &NodeId) -> bool {
        if !self.nodes.contains_key(id) {
            return false;
        }
        self.last_updated.insert(id.clone(), Instant::now());
        true
    }

    pub fn get_connected_nodes_json(
        &self,
        connected_ids: &std::collections::HashSet<NodeId>,
    ) -> String {
        let mut result = Vec::new();
        for n in self.nodes.values() {
            if connected_ids.contains(&n.id) {
                result.push(serde_json::json!({
                    "id": n.id.0, "x": n.position.x, "y": n.position.y, "z": n.position.z
                }));
            }
        }
        for id in connected_ids {
            if !self.nodes.contains_key(id) {
                result.push(serde_json::json!({
                    "id": id.0, "x": 0.0, "y": 0.0, "z": 0.0
                }));
            }
        }
        serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string())
    }

    pub fn get_all_nodes_json(&self, connected_ids: &std::collections::HashSet<NodeId>) -> String {
        let mut nodes_map = std::collections::HashMap::new();
        for n in self.nodes.values() {
            nodes_map.insert(
                n.id.clone(),
                serde_json::json!({
                    "id": n.id.0, "x": n.position.x, "y": n.position.y, "z": n.position.z
                }),
            );
        }
        for id in connected_ids {
            if !nodes_map.contains_key(id) {
                nodes_map.insert(
                    id.clone(),
                    serde_json::json!({
                        "id": id.0, "x": 0.0, "y": 0.0, "z": 0.0
                    }),
                );
            }
        }
        let list: Vec<serde_json::Value> = nodes_map.into_values().collect();
        serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string())
    }
    pub fn get_nodes_json(&self, ids: &std::collections::HashSet<NodeId>) -> String {
        let result: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                let (x, y, z) = self
                    .nodes
                    .get(id)
                    .map(|n| (n.position.x, n.position.y, n.position.z))
                    .unwrap_or((0.0, 0.0, 0.0));
                serde_json::json!({ "id": id.0, "x": x, "y": y, "z": z })
            })
            .collect();
        serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string())
    }

    /// Drops nodes whose `last_updated` timestamp is older than `duration`
    /// ("forgotten" -- e.g. the mutual-forgetting side-effect of DNVE3's
    /// 10s expiry). Logs one `[ConnTiming] kind=forgotten` line per node
    /// actually dropped, via plain `tracing::info!` -- this crate has no
    /// access to `mistlib-native`'s `conn_timing` rate limiter (and doesn't
    /// need one: expiry only ever removes each node once, so volume is
    /// naturally bounded).
    pub fn retain_recent(&mut self, duration: web_time::Duration) {
        let now = Instant::now();
        let mut expired: Vec<(NodeId, u64)> = Vec::new();
        self.last_updated.retain(|id, last| {
            let age = now.duration_since(*last);
            if age < duration {
                true
            } else {
                expired.push((id.clone(), age.as_millis() as u64));
                false
            }
        });
        for (id, age_ms) in &expired {
            tracing::info!(
                "[ConnTiming] peer={} kind=forgotten age_ms={}",
                id.0,
                age_ms
            );
        }
        if !expired.is_empty() {
            let expired_ids: std::collections::HashSet<&NodeId> =
                expired.iter().map(|(id, _)| id).collect();
            self.nodes.retain(|id, _| !expired_ids.contains(id));
        }
    }

    pub fn get_nodes_in_range(
        &self,
        center_id: &NodeId,
        range: f32,
    ) -> std::collections::HashSet<NodeId> {
        let mut in_range = std::collections::HashSet::new();
        if let Some(center_pos) = self.nodes.get(center_id).map(|n| n.position) {
            for (id, info) in &self.nodes {
                if id == center_id {
                    continue;
                }
                let dx = info.position.x - center_pos.x;
                let dy = info.position.y - center_pos.y;
                let dz = info.position.z - center_pos.z;
                let dist = (dx * dx + dy * dy + dz * dz).sqrt();
                if dist <= range {
                    in_range.insert(id.clone());
                }
            }
        }
        in_range
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_node_refreshes_known_node_only() {
        let mut store = NodeStore::new();
        let known = NodeId("known".to_string());
        let unknown = NodeId("unknown".to_string());

        store.update_node_position(known.clone(), Vector3::zero());
        assert!(store.touch_node(&known));
        assert!(store.last_updated.contains_key(&known));

        assert!(!store.touch_node(&unknown));
        assert!(!store.nodes.contains_key(&unknown));
        assert!(!store.last_updated.contains_key(&unknown));
    }

    #[test]
    fn retain_recent_drops_only_expired_nodes() {
        let mut store = NodeStore::new();
        let old = NodeId("old".to_string());
        let fresh = NodeId("fresh".to_string());

        store.update_node_position(old.clone(), Vector3::zero());
        // A zero-length window means anything already inserted counts as
        // expired ("forgotten"): `age < duration` can never hold once
        // `duration` is zero.
        store.retain_recent(web_time::Duration::from_millis(0));
        assert!(
            !store.nodes.contains_key(&old),
            "node past the retention window must be dropped"
        );
        assert!(!store.last_updated.contains_key(&old));

        store.update_node_position(fresh.clone(), Vector3::zero());
        store.retain_recent(web_time::Duration::from_secs(60));
        assert!(
            store.nodes.contains_key(&fresh),
            "node within the retention window must be kept"
        );
        assert!(store.last_updated.contains_key(&fresh));
    }
}
