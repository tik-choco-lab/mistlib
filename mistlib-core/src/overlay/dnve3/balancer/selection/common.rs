use crate::config::Config;
use crate::overlay::dnve3::balancer::{
    DNVE3ConnectionBalancer, DensityGuidance, DISTANCE_SCORE_WEIGHT, MIN_DISTANCE_THRESHOLD,
    PEER_DENSITY_SCORE_WEIGHT,
};
use crate::overlay::dnve3::spatial_density::Vector3;
use crate::types::NodeId;
use std::collections::HashSet;

/// Helpers shared by two or more of the per-mode selection algorithms in
/// the sibling `selection/*` modules.
impl DNVE3ConnectionBalancer {
    pub(super) fn extend_unique_nodes(
        &self,
        selected: &mut Vec<NodeId>,
        seen: &mut HashSet<NodeId>,
        nodes: Vec<NodeId>,
        max_selected: usize,
    ) {
        for id in nodes {
            if selected.len() >= max_selected {
                break;
            }
            if seen.insert(id.clone()) {
                selected.push(id);
            }
        }
    }

    pub(super) fn select_aoi_guard_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        limit: usize,
    ) -> Vec<NodeId> {
        if limit == 0 || config.dnve.aoi_range <= MIN_DISTANCE_THRESHOLD {
            return Vec::new();
        }

        let mut candidates: Vec<_> = all_nodes
            .iter()
            .filter_map(|(id, pos)| {
                let dist = (*pos - self_pos).magnitude();
                if !(MIN_DISTANCE_THRESHOLD..=config.dnve.aoi_range).contains(&dist) {
                    return None;
                }
                Some((id, dist))
            })
            .collect();
        candidates.sort_by(|(a_id, a_dist), (b_id, b_dist)| {
            a_dist.total_cmp(b_dist).then_with(|| a_id.0.cmp(&b_id.0))
        });
        candidates
            .into_iter()
            .take(limit)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub(super) fn select_density_guided_directional_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        guidance: &DensityGuidance,
        target: usize,
    ) -> Vec<NodeId> {
        let direction_threshold = config.effective_direction_threshold();
        let max_selected = target
            .max(1)
            .min(config.limits.max_connection_count as usize);
        let mut candidates = Vec::new();

        for (dir_index, dir) in self.spatial_utils.directions.iter().enumerate() {
            let direction_score = guidance.direction_score(dir_index);
            let best = all_nodes
                .iter()
                .filter_map(|(id, pos)| {
                    let vec = *pos - self_pos;
                    let dist = vec.magnitude();
                    if dist < MIN_DISTANCE_THRESHOLD {
                        return None;
                    }

                    let alignment = vec.normalized().dot(*dir);
                    if alignment < direction_threshold {
                        return None;
                    }

                    let peer_score = guidance.peer_score(id);
                    let candidate_score =
                        direction_score + peer_score * PEER_DENSITY_SCORE_WEIGHT + alignment
                            - dist * DISTANCE_SCORE_WEIGHT;
                    Some((id, dist, candidate_score))
                })
                .max_by(|(a_id, a_dist, a_score), (b_id, b_dist, b_score)| {
                    a_score
                        .total_cmp(b_score)
                        .then_with(|| b_dist.total_cmp(a_dist))
                        .then_with(|| b_id.0.cmp(&a_id.0))
                });

            if let Some((id, dist, score)) = best {
                candidates.push((id.clone(), direction_score, score, dist));
            }
        }

        candidates.sort_by(
            |(a_id, a_direction_score, a_score, a_dist),
             (b_id, b_direction_score, b_score, b_dist)| {
                b_direction_score
                    .total_cmp(a_direction_score)
                    .then_with(|| b_score.total_cmp(a_score))
                    .then_with(|| a_dist.total_cmp(b_dist))
                    .then_with(|| a_id.0.cmp(&b_id.0))
            },
        );

        let mut seen = HashSet::new();
        let mut selected = Vec::new();
        for (id, _, _, _) in candidates {
            if seen.insert(id.clone()) {
                selected.push(id);
                if selected.len() >= max_selected {
                    break;
                }
            }
        }

        selected
    }
}
