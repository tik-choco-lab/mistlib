use super::spatial_density::SpatialDensityData;
use crate::types::NodeId;
use std::collections::{HashMap, HashSet};
use web_time::{Duration, Instant};

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
    last_sent_heartbeat: Option<SpatialDensityData>,
    last_sent_heartbeat_at: Option<Instant>,
    last_sent_heartbeat_targets: HashSet<NodeId>,
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

    /// Decides whether a HEARTBEAT carrying `candidate` is actually worth
    /// sending, given what was last sent. Skips the send when the density data
    /// is byte-for-byte identical to last time *and* the same peers were the
    /// targets *and* `min_refresh` hasn't elapsed yet - the refresh floor exists
    /// because receivers prune density_peers by last-message time
    /// (`expire_node_seconds`), so a fully idle sender must still be heard from
    /// occasionally or it gets pruned as if it had disconnected. A newly
    /// connected peer (not in `last_sent_heartbeat_targets`) always forces a
    /// send so it isn't starved until the next change/refresh.
    ///
    /// Updates the "last sent" bookkeeping whenever it returns `true`.
    pub fn should_send_heartbeat(
        &mut self,
        candidate: &SpatialDensityData,
        connected_nodes: &[NodeId],
        min_refresh: Duration,
        now: Instant,
    ) -> bool {
        let connected: HashSet<NodeId> = connected_nodes.iter().cloned().collect();

        let unchanged = self
            .last_sent_heartbeat
            .as_ref()
            .is_some_and(|last| last == candidate);
        let fresh_enough = self
            .last_sent_heartbeat_at
            .is_some_and(|at| now.duration_since(at) < min_refresh);
        let same_targets = self.last_sent_heartbeat_targets == connected;

        let should_send = !(unchanged && fresh_enough && same_targets);

        if should_send {
            self.last_sent_heartbeat = Some(candidate.clone());
            self.last_sent_heartbeat_at = Some(now);
            self.last_sent_heartbeat_targets = connected;
        }

        should_send
    }
}
