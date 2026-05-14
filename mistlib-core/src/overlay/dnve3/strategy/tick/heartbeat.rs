use super::timer;
use crate::action::OverlayAction;
use crate::config::{Config, ConnectionMode};
use crate::overlay::dnve3::strategy::DNVE3Strategy;
use crate::stats::ping;
use crate::types::NodeId;
use web_time::Instant;

impl DNVE3Strategy {
    pub(super) fn tick_periodic(
        &self,
        config: &Config,
        now: Instant,
        connected_nodes: &[NodeId],
        mode: ConnectionMode,
    ) -> Vec<OverlayAction> {
        let interval = timer::heartbeat_interval(config);
        if !timer::is_due(
            &self.heartbeat_due_at,
            "heartbeat_due_at",
            now,
            now + interval,
            now + interval,
        ) {
            return Vec::new();
        }

        self.exchanger.delete_old_data(config);
        let mut actions = self.heartbeat_actions(config, connected_nodes, mode);
        actions.extend(ping::tick_actions(
            &self.local_node_id,
            config.limits.hop_count,
            connected_nodes,
        ));
        actions
    }

    fn heartbeat_actions(
        &self,
        config: &Config,
        connected_nodes: &[NodeId],
        mode: ConnectionMode,
    ) -> Vec<OverlayAction> {
        match mode {
            ConnectionMode::DirectionDensity
            | ConnectionMode::DirectionDensityLight
            | ConnectionMode::NodeListAoiDensity => self
                .exchanger
                .update_and_send_heartbeat(config, connected_nodes),
            ConnectionMode::NodeListDirectional
            | ConnectionMode::NodeListAoiGuard
            | ConnectionMode::NodeListAoiProximity
            | ConnectionMode::NodeListProximity
            | ConnectionMode::PSense => Vec::new(),
        }
    }
}
