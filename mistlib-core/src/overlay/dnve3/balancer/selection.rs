use super::{
    DNVE3ConnectionBalancer, DensityGuidance, AOI_GUARD_CONNECTIONS, DISTANCE_SCORE_WEIGHT,
    MIN_DISTANCE_THRESHOLD, PEER_DENSITY_SCORE_WEIGHT,
};
use crate::config::Config;
use crate::overlay::dnve3::spatial_density::Vector3;
use crate::types::NodeId;
use std::collections::HashSet;

const PSENSE_SENSOR_RESERVE_DIVISOR: usize = 4;
const PSENSE_MIN_SENSOR_RESERVE: usize = 1;

impl DNVE3ConnectionBalancer {
    pub(super) fn select_directional_nodes(
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

    pub(super) fn select_direction_density_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        guidance: Option<&DensityGuidance>,
        _target: usize,
    ) -> Vec<NodeId> {
        let max_selected = config.limits.max_connection_count as usize;
        if max_selected == 0 {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        let mut selected = Vec::new();

        self.extend_unique_nodes(
            &mut selected,
            &mut seen,
            self.select_aoi_guard_nodes(config, self_pos, all_nodes, AOI_GUARD_CONNECTIONS),
            max_selected,
        );
        self.extend_unique_nodes(
            &mut selected,
            &mut seen,
            self.select_directional_nodes(config, self_pos, all_nodes),
            max_selected,
        );

        if let Some(guidance) = guidance {
            self.extend_unique_nodes(
                &mut selected,
                &mut seen,
                self.select_density_guided_directional_nodes(
                    config,
                    self_pos,
                    all_nodes,
                    guidance,
                    max_selected,
                ),
                max_selected,
            );
        }

        selected
    }

    pub(super) fn select_direction_density_light_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        guidance: Option<&DensityGuidance>,
        _target: usize,
    ) -> Vec<NodeId> {
        let max_selected = config.limits.max_connection_count as usize;
        if max_selected == 0 {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        let mut selected = Vec::new();

        self.extend_unique_nodes(
            &mut selected,
            &mut seen,
            self.select_directional_nodes(config, self_pos, all_nodes),
            max_selected,
        );

        if let Some(guidance) = guidance {
            self.extend_unique_nodes(
                &mut selected,
                &mut seen,
                self.select_density_guided_directional_nodes(
                    config,
                    self_pos,
                    all_nodes,
                    guidance,
                    max_selected,
                ),
                max_selected,
            );
        }

        selected
    }

    pub(super) fn select_node_list_aoi_guard_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
    ) -> Vec<NodeId> {
        let max_selected = config.limits.max_connection_count as usize;
        if max_selected == 0 {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        let mut selected = Vec::new();

        self.extend_unique_nodes(
            &mut selected,
            &mut seen,
            self.select_aoi_guard_nodes(config, self_pos, all_nodes, AOI_GUARD_CONNECTIONS),
            max_selected,
        );
        self.extend_unique_nodes(
            &mut selected,
            &mut seen,
            self.select_directional_nodes(config, self_pos, all_nodes),
            max_selected,
        );

        selected
    }

    pub(super) fn select_node_list_aoi_proximity_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
    ) -> Vec<NodeId> {
        let max_selected = config.limits.max_connection_count as usize;
        if max_selected == 0 {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        let mut selected = Vec::new();

        self.extend_unique_nodes(
            &mut selected,
            &mut seen,
            self.select_aoi_guard_nodes(config, self_pos, all_nodes, AOI_GUARD_CONNECTIONS),
            max_selected,
        );
        self.extend_unique_nodes(
            &mut selected,
            &mut seen,
            self.select_proximity_nodes(self_pos, all_nodes, max_selected),
            max_selected,
        );

        selected
    }

    pub(super) fn select_node_list_aoi_density_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        guidance: Option<&DensityGuidance>,
        target: usize,
    ) -> Vec<NodeId> {
        let max_selected = config.limits.max_connection_count as usize;
        if max_selected == 0 {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        let mut selected = Vec::new();

        self.extend_unique_nodes(
            &mut selected,
            &mut seen,
            self.select_aoi_guard_nodes(config, self_pos, all_nodes, AOI_GUARD_CONNECTIONS),
            max_selected,
        );
        self.extend_unique_nodes(
            &mut selected,
            &mut seen,
            self.select_directional_nodes(config, self_pos, all_nodes),
            max_selected,
        );

        if let Some(guidance) = guidance {
            self.extend_unique_nodes(
                &mut selected,
                &mut seen,
                self.select_density_guided_directional_nodes(
                    config, self_pos, all_nodes, guidance, target,
                ),
                max_selected,
            );
        }

        selected
    }

    pub(super) fn select_p_sense_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
    ) -> Vec<NodeId> {
        let max_selected = config.limits.max_connection_count as usize;
        if max_selected == 0 {
            return Vec::new();
        }

        let mut seen = HashSet::new();
        let mut selected = Vec::new();
        let near_nodes = self.select_nodes_inside_aoi(config, self_pos, all_nodes);
        let sensor_nodes = self.select_p_sense_sensor_nodes(config, self_pos, all_nodes);
        let sensor_budget =
            Self::p_sense_sensor_budget(max_selected, near_nodes.len(), sensor_nodes.len());
        let near_budget = max_selected.saturating_sub(sensor_budget);

        self.extend_unique_nodes(&mut selected, &mut seen, near_nodes.clone(), near_budget);
        let sensor_cap = selected
            .len()
            .saturating_add(sensor_budget)
            .min(max_selected);
        self.extend_unique_nodes(&mut selected, &mut seen, sensor_nodes.clone(), sensor_cap);
        self.extend_unique_nodes(&mut selected, &mut seen, near_nodes, max_selected);
        self.extend_unique_nodes(&mut selected, &mut seen, sensor_nodes, max_selected);

        selected
    }

    fn p_sense_sensor_budget(max_selected: usize, near_count: usize, sensor_count: usize) -> usize {
        if max_selected <= 1 || sensor_count == 0 {
            return 0;
        }

        let reserve_limit = if near_count == 0 {
            max_selected
        } else {
            max_selected - 1
        };
        let reserve = (max_selected / PSENSE_SENSOR_RESERVE_DIVISOR)
            .max(PSENSE_MIN_SENSOR_RESERVE)
            .min(reserve_limit);

        reserve.min(sensor_count)
    }

    fn extend_unique_nodes(
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

    fn select_aoi_guard_nodes(
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

    fn select_nodes_inside_aoi(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
    ) -> Vec<NodeId> {
        self.select_aoi_guard_nodes(config, self_pos, all_nodes, usize::MAX)
    }

    fn select_p_sense_sensor_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
    ) -> Vec<NodeId> {
        let direction_threshold = config.effective_direction_threshold();
        let mut sensors = Vec::new();

        for dir in &self.spatial_utils.directions {
            let closest_outside_aoi = all_nodes
                .iter()
                .filter_map(|(id, pos)| {
                    let vec = *pos - self_pos;
                    let dist = vec.magnitude();
                    if dist <= config.dnve.aoi_range || dist < MIN_DISTANCE_THRESHOLD {
                        return None;
                    }
                    if vec.normalized().dot(*dir) < direction_threshold {
                        return None;
                    }
                    Some((id, dist))
                })
                .min_by(|(a_id, a_dist), (b_id, b_dist)| {
                    a_dist.total_cmp(b_dist).then_with(|| a_id.0.cmp(&b_id.0))
                });

            if let Some((id, _)) = closest_outside_aoi {
                sensors.push(id.clone());
            }
        }

        sensors
    }

    fn select_density_guided_directional_nodes(
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

    /// 近距離順に最大 `target` 件のノードを選ぶ（方向制約なし）
    pub(super) fn select_proximity_nodes(
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
