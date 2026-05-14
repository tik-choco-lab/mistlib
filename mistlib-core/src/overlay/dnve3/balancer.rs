mod actions;
mod guidance;
mod selection;

use crate::action::OverlayAction;
use crate::config::{Config, ConnectionMode};
use crate::overlay::dnve3::spatial_density::{SpatialDensityUtils, Vector3};
use crate::types::{ConnectionState, NodeId};
use std::collections::{HashMap, HashSet};

pub(crate) use guidance::DensityGuidance;

const MIN_DISTANCE_THRESHOLD: f32 = 0.001;
const AOI_GUARD_CONNECTIONS: usize = 5;
const DISTANCE_SCORE_WEIGHT: f32 = 1.0;
const PEER_DENSITY_SCORE_WEIGHT: f32 = 0.25;

pub struct DNVE3ConnectionBalancer {
    spatial_utils: SpatialDensityUtils,
}

struct BalancerActionContext<'a> {
    selected_ids: &'a HashSet<&'a NodeId>,
    self_pos: Vector3,
    node_positions: &'a HashMap<&'a NodeId, Vector3>,
    connected_map: &'a HashMap<&'a NodeId, ConnectionState>,
    config: &'a Config,
    self_id: &'a NodeId,
    target: usize,
}

impl DNVE3ConnectionBalancer {
    pub fn new(config: &Config) -> Self {
        Self {
            spatial_utils: SpatialDensityUtils::new_with_partition(
                config.dnve.density_resolution as usize,
                config.dnve.spatial_partition_type,
            ),
        }
    }

    pub fn select_connections(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        connected_node_states: &[(NodeId, ConnectionState)],
        self_id: &NodeId,
    ) -> Vec<OverlayAction> {
        self.select_connections_with_density_guidance(
            config,
            self_pos,
            all_nodes,
            connected_node_states,
            self_id,
            None,
        )
    }

    pub(crate) fn select_connections_with_density_guidance(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        connected_node_states: &[(NodeId, ConnectionState)],
        self_id: &NodeId,
        density_guidance: Option<&DensityGuidance>,
    ) -> Vec<OverlayAction> {
        let target = config
            .limits
            .max_connection_count
            .saturating_sub(config.limits.reserved_connection_count) as usize;

        let selected_nodes = match config.dnve.connection_mode {
            ConnectionMode::DirectionDensity => self.select_direction_density_nodes(
                config,
                self_pos,
                all_nodes,
                density_guidance.filter(|g| g.has_signal()),
                target,
            ),
            ConnectionMode::DirectionDensityLight => self.select_direction_density_light_nodes(
                config,
                self_pos,
                all_nodes,
                density_guidance.filter(|g| g.has_signal()),
                target,
            ),
            ConnectionMode::NodeListDirectional => {
                self.select_directional_nodes(config, self_pos, all_nodes)
            }
            ConnectionMode::NodeListAoiGuard => {
                self.select_node_list_aoi_guard_nodes(config, self_pos, all_nodes)
            }
            ConnectionMode::NodeListAoiProximity => {
                self.select_node_list_aoi_proximity_nodes(config, self_pos, all_nodes)
            }
            ConnectionMode::NodeListAoiDensity => self.select_node_list_aoi_density_nodes(
                config,
                self_pos,
                all_nodes,
                density_guidance.filter(|g| g.has_signal()),
                target,
            ),
            ConnectionMode::NodeListProximity => self.select_proximity_nodes(
                self_pos,
                all_nodes,
                config.limits.max_connection_count as usize,
            ),
            ConnectionMode::PSense => self.select_p_sense_nodes(config, self_pos, all_nodes),
        };
        let node_positions: HashMap<&NodeId, Vector3> =
            all_nodes.iter().map(|(id, pos)| (id, *pos)).collect();

        let mut connected: Vec<NodeId> = connected_node_states
            .iter()
            .filter(|(_, s)| *s == ConnectionState::Connected)
            .map(|(id, _)| id.clone())
            .collect();
        let connected_map: HashMap<&NodeId, ConnectionState> = connected_node_states
            .iter()
            .map(|(id, s)| (id, *s))
            .collect();

        let selected_ids: HashSet<&NodeId> = selected_nodes.iter().collect();

        let ctx = BalancerActionContext {
            selected_ids: &selected_ids,
            self_pos,
            node_positions: &node_positions,
            connected_map: &connected_map,
            config,
            self_id,
            target,
        };

        let mut actions = self.prune_excess_connections(&mut connected, &ctx);
        actions.extend(self.connect_selected_nodes(&mut connected, &selected_nodes, &ctx));
        actions
    }
}
