use crate::types::NodeId;
use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub(crate) struct DensityGuidance {
    pub direction_scores: Vec<f32>,
    pub peer_scores: HashMap<NodeId, f32>,
}

impl DensityGuidance {
    pub fn has_signal(&self) -> bool {
        self.direction_scores.iter().any(|score| *score > 0.0)
            || self.peer_scores.values().any(|score| *score > 0.0)
    }

    pub(super) fn direction_score(&self, index: usize) -> f32 {
        self.direction_scores
            .get(index)
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn peer_score(&self, id: &NodeId) -> f32 {
        self.peer_scores.get(id).copied().unwrap_or_default()
    }
}
