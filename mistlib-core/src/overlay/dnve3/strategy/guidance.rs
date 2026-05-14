use super::DNVE3Strategy;
use crate::config::{Config, ConnectionMode};
use crate::overlay::dnve3::{balancer::DensityGuidance, DNVE3DataStore};
use crate::types::NodeId;
use std::collections::HashMap;

impl DNVE3Strategy {
    pub(super) fn maybe_density_guidance(
        &self,
        config: &Config,
        mode: ConnectionMode,
    ) -> Option<DensityGuidance> {
        matches!(
            mode,
            ConnectionMode::DirectionDensity
                | ConnectionMode::DirectionDensityLight
                | ConnectionMode::NodeListAoiDensity
        )
        .then(|| self.density_guidance(config))
        .flatten()
    }

    fn density_guidance(&self, config: &Config) -> Option<DensityGuidance> {
        let store = self.data_store.lock().expect("data_store lock poisoned");
        let guidance = DensityGuidance {
            direction_scores: self.direction_density_scores(config, &store),
            peer_scores: Self::peer_density_scores(&store),
        };

        guidance.has_signal().then_some(guidance)
    }

    fn direction_density_scores(&self, config: &Config, store: &DNVE3DataStore) -> Vec<f32> {
        let direction_count = config
            .dnve
            .spatial_partition_type
            .direction_count(config.dnve.density_resolution) as usize;
        let fallback_layer_count = config.dnve.distance_layers.max(1) as usize;
        let mut direction_scores = vec![0.0; direction_count];

        if let Some(merged_density) = &store.merged_density_map {
            let layer_count = store
                .self_density
                .as_ref()
                .map(|data| data.layer_count.max(1))
                .unwrap_or(fallback_layer_count);
            let self_map = store
                .self_density
                .as_ref()
                .map(|data| data.density_map.as_slice());

            for (dir_index, direction_score) in direction_scores.iter_mut().enumerate() {
                let merged_score =
                    Self::sum_direction_density(merged_density, dir_index, layer_count);
                let self_score = self_map
                    .map(|map| Self::sum_direction_density(map, dir_index, layer_count))
                    .unwrap_or_default();
                *direction_score = (merged_score - self_score).max(0.0);
            }
        }

        direction_scores
    }

    fn peer_density_scores(store: &DNVE3DataStore) -> HashMap<NodeId, f32> {
        let mut peer_scores = HashMap::new();

        for (id, info) in &store.density_peers {
            let score = info
                .data
                .density_map
                .iter()
                .copied()
                .filter(|value| *value > 0.0)
                .sum();
            peer_scores.insert(id.clone(), score);
        }

        peer_scores
    }

    fn sum_direction_density(map: &[f32], dir_index: usize, layer_count: usize) -> f32 {
        let start = dir_index.saturating_mul(layer_count);
        if start >= map.len() {
            return 0.0;
        }
        let end = start.saturating_add(layer_count).min(map.len());
        map[start..end]
            .iter()
            .copied()
            .filter(|value| *value > 0.0)
            .sum()
    }
}
