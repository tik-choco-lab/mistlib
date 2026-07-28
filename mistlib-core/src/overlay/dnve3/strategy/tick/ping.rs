use super::timer;
use crate::action::OverlayAction;
use crate::config::Config;
use crate::overlay::dnve3::strategy::DNVE3Strategy;
use crate::stats::ping;
use crate::types::NodeId;
use web_time::Instant;

impl DNVE3Strategy {
    /// PING keepalive runs on its own timer (`intervals.ping`), decoupled from the
    /// heartbeat/node_list cadence so it keeps a steady liveness-detection rate
    /// even while those back off when the room is idle.
    pub(super) fn tick_ping(
        &self,
        config: &Config,
        now: Instant,
        connected_nodes: &[NodeId],
    ) -> Vec<OverlayAction> {
        let interval = timer::ping_interval(config);
        if !timer::is_due(
            &self.ping_due_at,
            "ping_due_at",
            now,
            now + interval,
            now + interval,
        ) {
            return Vec::new();
        }

        ping::tick_actions(
            &self.local_node_id,
            config.limits.hop_count,
            connected_nodes,
            config.limits.ping_timeout_count,
        )
    }
}
