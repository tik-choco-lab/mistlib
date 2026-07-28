use crate::config::Config;
use crate::overlay::dnve3::balancer::{DNVE3ConnectionBalancer, DensityGuidance};
use crate::overlay::dnve3::spatial_density::Vector3;
use crate::types::NodeId;
use std::collections::HashSet;

impl DNVE3ConnectionBalancer {
    pub(in crate::overlay::dnve3::balancer) fn select_direction_density_light_nodes(
        &self,
        config: &Config,
        self_pos: Vector3,
        all_nodes: &[(NodeId, Vector3)],
        guidance: Option<&DensityGuidance>,
        _target: usize,
    ) -> Vec<NodeId> {
        let max_selected = config.limits.max_connection_count as usize;
        if max_selected == 0 {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        let mut selected = Vec::new();

        self.extend_unique_nodes(
            &mut selected,
            &mut seen,
            self.select_directional_nodes(config, self_pos, all_nodes),
            max_selected,
        );

        if let Some(guidance) = guidance {
            self.extend_unique_nodes(
                &mut selected,
                &mut seen,
                self.select_density_guided_directional_nodes(
                    config,
                    self_pos,
                    all_nodes,
                    guidance,
                    max_selected,
                ),
                max_selected,
            );
        }

        selected
    }
}
