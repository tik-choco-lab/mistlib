use crate::config::Config;
use crate::overlay::dnve3::balancer::{DNVE3ConnectionBalancer, MIN_DISTANCE_THRESHOLD};
use crate::overlay::dnve3::spatial_density::Vector3;
use crate::types::NodeId;
use std::collections::HashSet;

const PSENSE_SENSOR_RESERVE_DIVISOR: usize = 4;
const PSENSE_MIN_SENSOR_RESERVE: usize = 1;

impl DNVE3ConnectionBalancer {
    pub(in crate::overlay::dnve3::balancer) fn select_p_sense_nodes(
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
}
