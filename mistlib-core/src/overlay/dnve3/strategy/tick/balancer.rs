use super::timer;
use crate::action::OverlayAction;
use crate::config::Config;
use crate::overlay::dnve3::balancer::DensityGuidance;
use crate::overlay::dnve3::strategy::DNVE3Strategy;
use crate::types::{ConnectionState, NodeId};
use web_time::Instant;

impl DNVE3Strategy {
    pub(super) fn tick_balancer(
        &self,
        config: &Config,
        now: Instant,
        connected_node_states: &[(NodeId, ConnectionState)],
        density_guidance: Option<&DensityGuidance>,
    ) -> Vec<OverlayAction> {
        if !timer::is_due(
            &self.balancer_due_at,
            "balancer_due_at",
            now,
            now,
            now + timer::balancer_interval_with_jitter(config),
        ) {
            return Vec::new();
        }

        self.run_select_connection(config, connected_node_states, density_guidance)
    }

    /// Runs a connection re-selection immediately, bypassing the periodic
    /// balancer timer, and pushes the next periodic tick out from now so it
    /// doesn't fire again moments later. Used when a peer disconnect is
    /// confirmed, so recovery doesn't wait for the next tick window.
    pub(super) fn force_immediate_balancer_tick(
        &self,
        config: &Config,
        connected_node_states: &[(NodeId, ConnectionState)],
        density_guidance: Option<&DensityGuidance>,
    ) -> Vec<OverlayAction> {
        let next_due_at = Instant::now() + timer::balancer_interval_with_jitter(config);
        *self
            .balancer_due_at
            .lock()
            .expect("balancer_due_at lock poisoned") = Some(next_due_at);

        self.run_select_connection(config, connected_node_states, density_guidance)
    }
}
