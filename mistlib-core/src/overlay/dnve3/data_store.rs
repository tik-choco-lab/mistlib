use super::spatial_density::SpatialDensityData;
use crate::types::NodeId;
use std::collections::HashMap;
use web_time::Instant;

pub struct NeighborDensityInfo {
    pub data: SpatialDensityData,
    pub last_message_time: Instant,
}

pub struct DNVE3DataStore {
    pub self_density: Option<SpatialDensityData>,
    pub merged_density_map: Option<Vec<f32>>,
    pub neighbors: HashMap<NodeId, NeighborDensityInfo>,
}

impl DNVE3DataStore {
    pub fn new() -> Self {
        Self {
            self_density: None,
            merged_density_map: None,
            neighbors: HashMap::new(),
        }
    }

    pub fn add_or_update_neighbor(&mut self, id: NodeId, data: SpatialDensityData) {
        self.neighbors.insert(
            id,
            NeighborDensityInfo {
                data,
                last_message_time: Instant::now(),
            },
        );
    }

    pub fn remove_neighbor(&mut self, id: &NodeId) {
        self.neighbors.remove(id);
    }

    pub fn update_last_message_time(&mut self, id: &NodeId) {
        if let Some(info) = self.neighbors.get_mut(id) {
            info.last_message_time = Instant::now();
        }
    }
}
