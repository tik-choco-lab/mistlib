use super::DNVE3Strategy;
use crate::action::OverlayAction;
use crate::config::Config;
use crate::overlay::dnve3::{balancer::DensityGuidance, spatial_density::Vector3};
use crate::types::{ConnectionState, NodeId};

struct SelectionInput {
    self_pos: Vector3,
    known_nodes: Vec<(NodeId, Vector3)>,
}

impl DNVE3Strategy {
    pub(super) fn run_select_connection(
        &self,
        config: &Config,
        connected_node_states: &[(NodeId, ConnectionState)],
        density_guidance: Option<&DensityGuidance>,
    ) -> Vec<OverlayAction> {
        let input = self.selection_input();
        Self::log_selection_input(&input, connected_node_states);
        let actions = self.balancer.select_connections_with_density_guidance(
            config,
            input.self_pos,
            &input.known_nodes,
            connected_node_states,
            &self.local_node_id,
            density_guidance,
        );
        Self::log_selection_actions(&actions);
        actions
    }

    fn selection_input(&self) -> SelectionInput {
        let store = self.node_store.lock().expect("node_store lock poisoned");
        let self_pos = store
            .nodes
            .get(&self.local_node_id)
            .map(|n| n.position)
            .unwrap_or_else(Vector3::zero);
        let known_nodes = store
            .nodes
            .iter()
            .filter(|(id, _)| *id != &self.local_node_id)
            .map(|(id, n)| (id.clone(), n.position))
            .collect();

        SelectionInput {
            self_pos,
            known_nodes,
        }
    }

    fn log_selection_input(
        input: &SelectionInput,
        connected_node_states: &[(NodeId, ConnectionState)],
    ) {
        tracing::debug!(
            "[DNVE3] SelectConn: self=({:.1},{:.1},{:.1}) known_nodes={} connected={}",
            input.self_pos.x,
            input.self_pos.y,
            input.self_pos.z,
            input.known_nodes.len(),
            connected_node_states.len()
        );
    }

    fn log_selection_actions(actions: &[OverlayAction]) {
        for action in actions {
            match action {
                OverlayAction::Connect { to } => {
                    tracing::debug!("[DNVE3] SelectConn: CONNECT -> {}", to.0);
                }
                OverlayAction::Disconnect { to } => {
                    tracing::debug!("[DNVE3] SelectConn: DISCONNECT -> {}", to.0);
                }
                _ => {}
            }
        }
    }
}
