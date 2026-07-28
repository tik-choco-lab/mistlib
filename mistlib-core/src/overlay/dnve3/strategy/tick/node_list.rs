use super::timer;
use crate::action::OverlayAction;
use crate::config::{Config, ConnectionMode, NodeListExchangeMode};
use crate::overlay::dnve3::balancer::DensityGuidance;
use crate::overlay::dnve3::spatial_density::Vector3;
use crate::overlay::dnve3::strategy::DNVE3Strategy;
use crate::types::NodeId;
use std::collections::HashSet;
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
        let base_interval = timer::node_list_interval_with_jitter(config);
        let multiplier = self.node_list_adaptive.current_multiplier();
        let interval = base_interval.mul_f32(multiplier);
        if !timer::is_due(
            &self.node_list_due_at,
            "node_list_due_at",
            now,
            now + interval,
            now + interval,
        ) {
            return Vec::new();
        }

        self.observe_node_list_round(config, connected_nodes);

        tracing::debug!(
            "[DNVE3] NodeList: exchange with {} peers (interval multiplier={multiplier:.1})",
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

    /// Feeds the current world snapshot to the adaptive interval tracker. Called
    /// right as a node_list round is about to happen, using the state known
    /// *before* this round's responses can possibly have arrived, so the
    /// comparison against the previous round approximates "did anything change
    /// since we last looked".
    fn observe_node_list_round(&self, config: &Config, connected_nodes: &[NodeId]) {
        let (self_pos, known_ids) = {
            let store = self.node_store.lock().expect("node_store lock poisoned");
            let self_pos = store
                .nodes
                .get(&self.local_node_id)
                .map(|n| n.position)
                .unwrap_or_else(Vector3::zero);
            let known_ids: HashSet<NodeId> = store.nodes.keys().cloned().collect();
            (self_pos, known_ids)
        };

        self.node_list_adaptive.observe(
            self_pos,
            connected_nodes,
            known_ids,
            config.dnve.aoi_range,
        );
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
