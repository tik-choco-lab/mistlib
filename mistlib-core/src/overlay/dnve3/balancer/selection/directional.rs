use crate::config::Config;
use crate::overlay::dnve3::balancer::{DNVE3ConnectionBalancer, MIN_DISTANCE_THRESHOLD};
use crate::overlay::dnve3::spatial_density::Vector3;
use crate::types::NodeId;
use std::collections::HashSet;

impl DNVE3ConnectionBalancer {
    pub(in crate::overlay::dnve3::balancer) fn select_directional_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
    ) -> Vec<NodeId> {
        let direction_threshold = config.effective_direction_threshold();
        let mut seen = HashSet::new();
        let mut selected = Vec::new();

        for dir in &self.spatial_utils.directions {
            let closest = all_nodes
                .iter()
                .filter_map(|(id, pos)| {
                    let vec = *pos - self_pos;
                    let dist = vec.magnitude();
                    if dist < MIN_DISTANCE_THRESHOLD {
                        return None;
                    }
                    if vec.normalized().dot(*dir) < direction_threshold {
                        return None;
                    }
                    Some((id, dist))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(id, _)| id);

            if let Some(id) = closest {
                if seen.insert(id) {
                    selected.push(id.clone());
                }
            }
        }

        selected
    }
}
