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

impl NodeStore {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            last_updated: HashMap::new(),
        }
    }

    pub fn update_node_position(&mut self, id: NodeId, position: Vector3) {
        self.nodes.insert(
            id.clone(),
            NodeInfo {
                id: id.clone(),
                position,
            },
        );
        self.last_updated.insert(id, Instant::now());
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
    pub fn retain_recent(&mut self, duration: web_time::Duration) {
        let now = Instant::now();
        self.nodes.retain(|id, _| {
            if let Some(last) = self.last_updated.get(id) {
                now.duration_since(*last) < duration
            } else {
                true
            }
        });
        self.last_updated
            .retain(|_, last| now.duration_since(*last) < duration);
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
