use super::spatial_density::SpatialDensityData;
use crate::types::NodeId;
use std::collections::{HashMap, HashSet};
use web_time::Instant;

pub struct DensityPeerInfo {
    pub data: SpatialDensityData,
    pub last_message_time: Instant,
}

#[derive(Default)]
pub struct DNVE3DataStore {
    pub self_density: Option<SpatialDensityData>,
    pub merged_density_map: Option<Vec<f32>>,
    pub density_peers: HashMap<NodeId, DensityPeerInfo>,
    pub aoi_nodes: HashSet<NodeId>,
}

impl DNVE3DataStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_or_update_neighbor(&mut self, id: NodeId, data: SpatialDensityData) {
        self.density_peers.insert(
            id,
            DensityPeerInfo {
                data,
                last_message_time: Instant::now(),
            },
        );
    }

    pub fn remove_neighbor(&mut self, id: &NodeId) {
        self.density_peers.remove(id);
    }

    pub fn update_last_message_time(&mut self, id: &NodeId) {
        if let Some(info) = self.density_peers.get_mut(id) {
            info.last_message_time = Instant::now();
        }
    }
}
