use super::timer;
use crate::action::OverlayAction;
use crate::config::{Config, ConnectionMode, NodeListExchangeMode};
use crate::overlay::dnve3::balancer::DensityGuidance;
use crate::overlay::dnve3::strategy::DNVE3Strategy;
use crate::types::NodeId;
use web_time::Instant;

impl DNVE3Strategy {
    pub(super) fn tick_node_list(
        &self,
        config: &Config,
        now: Instant,
        connected_nodes: &[NodeId],
        mode: ConnectionMode,
        density_guidance: Option<&DensityGuidance>,
    ) -> Vec<OverlayAction> {
        let interval = timer::node_list_interval(config);
        if !timer::is_due(
            &self.node_list_due_at,
            "node_list_due_at",
            now,
            now + interval,
            now + interval,
        ) {
            return Vec::new();
        }

        tracing::debug!(
            "[DNVE3] NodeList: exchange with {} peers",
            connected_nodes.len()
        );
        let targets = Self::node_list_targets(mode, connected_nodes, density_guidance);
        match (mode, config.dnve.node_list_exchange_mode) {
            (ConnectionMode::DirectionDensity, _) => targets
                .iter()
                .flat_map(|node_id| self.exchanger.send_request_node_list(node_id))
                .collect(),
            (_, NodeListExchangeMode::Pull) => targets
                .iter()
                .flat_map(|node_id| self.exchanger.send_request_node_list(node_id))
                .collect(),
            (_, NodeListExchangeMode::Push) => targets
                .iter()
                .flat_map(|node_id| self.exchanger.send_node_list_push(node_id, mode))
                .collect(),
        }
    }

    fn node_list_targets(
        mode: ConnectionMode,
        connected_nodes: &[NodeId],
        density_guidance: Option<&DensityGuidance>,
    ) -> Vec<NodeId> {
        let mut targets = connected_nodes.to_vec();
        if mode != ConnectionMode::DirectionDensity {
            return targets;
        }

        let Some(guidance) = density_guidance.filter(|g| g.has_signal()) else {
            return targets;
        };
        targets.sort_by(|a, b| {
            guidance
                .peer_scores
                .get(b)
                .copied()
                .unwrap_or_default()
                .total_cmp(&guidance.peer_scores.get(a).copied().unwrap_or_default())
                .then_with(|| a.0.cmp(&b.0))
        });
        targets
    }
}
