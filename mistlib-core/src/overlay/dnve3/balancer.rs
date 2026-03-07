use crate::action::OverlayAction;
use crate::config::Config;
use crate::overlay::dnve3::data_store::DNVE3DataStore;
use crate::overlay::dnve3::spatial_density::{SpatialDensityUtils, Vector3};
use crate::types::{ConnectionState, NodeId};
use std::sync::{Arc, Mutex};

const SCORE_SELECTED: f32 = 10000.0;
const MIN_DISTANCE_THRESHOLD: f32 = 0.001;
const BASE_WEIGHT: f32 = 1.0;
const PENALTY_NO_MESSAGE_HISTORY: f32 = 1000.0;

#[derive(Clone)]
pub struct DNVE3ConnectionBalancer {
    dnve_data_store: Arc<Mutex<DNVE3DataStore>>,
    spatial_utils: SpatialDensityUtils,
}

impl DNVE3ConnectionBalancer {
    pub fn new(dnve_data_store: Arc<Mutex<DNVE3DataStore>>, config: &Config) -> Self {
        Self {
            dnve_data_store,
            spatial_utils: SpatialDensityUtils::new(config.dnve.density_resolution as usize),
        }
    }

    pub fn find_important_nodes(&self) -> Vec<(NodeId, f32)> {
        let store = self.dnve_data_store.lock().unwrap();
        let self_density = match &store.self_density {
            Some(d) => d,
            None => return vec![],
        };

        let mut important_nodes = Vec::new();

        for (node_id, info) in &store.neighbors {
            let other_data = &info.data;
            let projected = self
                .spatial_utils
                .project_spatial_density(other_data, self_density.position);

            let mut score = 0.0;
            let layer_count = self_density.layer_count;
            let dir_count = self_density.dir_count;

            for j in 0..layer_count {
                let weight = BASE_WEIGHT / (BASE_WEIGHT + j as f32);
                for i in 0..dir_count {
                    let idx = i * layer_count + j;
                    let diff = projected.density_map[idx] - self_density.density_map[idx];
                    if diff > 0.0 {
                        score += diff * weight;
                    }
                }
            }

            if score > 0.0 {
                important_nodes.push((node_id.clone(), score));
            }
        }

        important_nodes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        important_nodes
    }

    pub fn select_connections(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        connected_node_states: &[(NodeId, ConnectionState)],
        self_id: &NodeId,
    ) -> Vec<OverlayAction> {
        let mut actions = Vec::new();

        let selected_nodes = self.select_directional_nodes(config, self_pos, all_nodes);
        let selected_node_ids: std::collections::HashSet<NodeId> =
            selected_nodes.iter().cloned().collect();

        let mut connected_nodes: Vec<NodeId> = connected_node_states
            .iter()
            .filter(|(_, s)| *s == ConnectionState::Connected || *s == ConnectionState::Connecting)
            .map(|(id, _)| id.clone())
            .collect();

        let connected_map: std::collections::HashMap<NodeId, ConnectionState> =
            connected_node_states.iter().cloned().collect();

        let target_count = config
            .limits
            .max_connection_count
            .saturating_sub(config.limits.reserved_connection_count)
            as usize;

        if connected_nodes.len() > target_count {
            let num_to_disconnect = (connected_nodes.len() - target_count)
                + config.limits.force_disconnect_count as usize;

            let mut scores = self.score_nodes(
                &connected_nodes,
                &selected_node_ids,
                self_pos,
                all_nodes,
                config,
            );
            scores.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            let mut disconnected_count = 0;
            for (id, _) in scores {
                if disconnected_count >= num_to_disconnect {
                    break;
                }
                if &id == self_id {
                    continue;
                }

                match connected_map.get(&id) {
                    Some(s)
                        if *s == ConnectionState::Connecting
                            || *s == ConnectionState::Connected => {}
                    _ => continue,
                }

                actions.push(OverlayAction::Disconnect { to: id.clone() });
                connected_nodes.retain(|x| x != &id);
                disconnected_count += 1;
            }
        }

        for id in &selected_nodes {
            if id == self_id {
                continue;
            }

            match connected_map.get(id) {
                Some(s)
                    if *s == ConnectionState::Connecting || *s == ConnectionState::Connected =>
                {
                    continue;
                }
                _ => {}
            }

            if connected_nodes.len() >= config.limits.max_connection_count as usize {

                let mut scores = self.score_nodes(
                    &connected_nodes,
                    &selected_node_ids,
                    self_pos,
                    all_nodes,
                    config,
                );
                scores.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

                if let Some((worst_id, _)) = scores.first() {
                    actions.push(OverlayAction::Disconnect {
                        to: worst_id.clone(),
                    });
                    let worst_id = worst_id.clone();
                    connected_nodes.retain(|x| x != &worst_id);
                }
            }

            actions.push(OverlayAction::Connect { to: id.clone() });
            connected_nodes.push(id.clone());
        }

        actions
    }

    fn select_directional_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
    ) -> Vec<NodeId> {
        let directions = &self.spatial_utils.directions;
        let direction_threshold = config.dnve.direction_threshold;
        let mut selected = Vec::new();

        for dir in directions {
            let mut closest: Option<&NodeId> = None;
            let mut min_dist = f32::MAX;

            for (id, pos) in all_nodes {
                let vec = *pos - self_pos;
                let dist = vec.magnitude();
                if dist < MIN_DISTANCE_THRESHOLD {
                    continue;
                }
                let dot = vec.normalized().dot(*dir);
                if dot < direction_threshold {
                    continue;
                }
                if dist < min_dist {
                    min_dist = dist;
                    closest = Some(id);
                }
            }

            if let Some(id) = closest {
                if !selected.contains(id) {
                    selected.push(id.clone());
                }
            }
        }

        selected
    }

    fn calculate_node_score(
        &self,
        id: &NodeId,
        selected_node_ids: &std::collections::HashSet<NodeId>,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        config: &Config,
    ) -> f32 {
        let mut score = 0.0;

        if selected_node_ids.contains(id) {
            score += SCORE_SELECTED;
        }

        if let Some((_, pos)) = all_nodes.iter().find(|(nid, _)| nid == id) {
            let dist = self_pos.dist(*pos);
            if dist <= config.dnve.aoi_range {
                score += 0f32.max(config.dnve.aoi_range - dist);
            }
        }

        let store = self.dnve_data_store.lock().unwrap();
        if let Some(info) = store.neighbors.get(id) {
            let elapsed = info.last_message_time.elapsed().as_secs_f32();
            score -= elapsed;
        } else {
            score -= PENALTY_NO_MESSAGE_HISTORY;
        }

        score
    }

    fn score_nodes(
        &self,
        nodes: &[NodeId],
        selected_node_ids: &std::collections::HashSet<NodeId>,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        config: &Config,
    ) -> Vec<(NodeId, f32)> {
        nodes
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    self.calculate_node_score(id, selected_node_ids, self_pos, all_nodes, config),
                )
            })
            .collect()
    }
}
