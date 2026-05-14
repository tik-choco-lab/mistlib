mod balancer;
mod heartbeat;
mod node_list;
mod timer;

use super::DNVE3Strategy;
use crate::action::OverlayAction;
use crate::config::{Config, ConnectionMode};
use crate::overlay::dnve3::balancer::DensityGuidance;
use crate::types::{ConnectionState, NodeId};
use web_time::Instant;

impl DNVE3Strategy {
    pub(super) fn connected_node_ids(
        connected_node_states: &[(NodeId, ConnectionState)],
    ) -> Vec<NodeId> {
        connected_node_states
            .iter()
            .filter(|(_, state)| *state == ConnectionState::Connected)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub(super) fn collect_tick_actions(
        &self,
        config: &Config,
        now: Instant,
        connected_node_states: &[(NodeId, ConnectionState)],
        connected_nodes: &[NodeId],
        mode: ConnectionMode,
        density_guidance: Option<&DensityGuidance>,
    ) -> Vec<OverlayAction> {
        let mut actions = self.tick_balancer(config, now, connected_node_states, density_guidance);
        actions.extend(self.tick_periodic(config, now, connected_nodes, mode));
        actions.extend(self.tick_node_list(config, now, connected_nodes, mode, density_guidance));
        actions
    }
}
