use crate::overlay::dnve3::balancer::{DNVE3ConnectionBalancer, MIN_DISTANCE_THRESHOLD};
use crate::overlay::dnve3::spatial_density::Vector3;
use crate::types::NodeId;

impl DNVE3ConnectionBalancer {
    /// 近距離順に最大 `target` 件のノードを選ぶ（方向制約なし）
    pub(in crate::overlay::dnve3::balancer) fn select_proximity_nodes(
        &self,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        target: usize,
    ) -> Vec<NodeId> {
        let mut distances: Vec<(&NodeId, f32)> = all_nodes
            .iter()
            .filter_map(|(id, pos)| {
                let dist = (*pos - self_pos).magnitude();
                if dist < MIN_DISTANCE_THRESHOLD {
                    None
                } else {
                    Some((id, dist))
                }
            })
            .collect();

        distances.sort_by(|a, b| a.1.total_cmp(&b.1));
        distances.truncate(target);
        distances.into_iter().map(|(id, _)| id.clone()).collect()
    }
}
