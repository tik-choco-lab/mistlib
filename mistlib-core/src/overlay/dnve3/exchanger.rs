mod heartbeat;
mod node_list;
mod position;

use crate::config::Config;
use crate::overlay::dnve3::data_store::DNVE3DataStore;
use crate::overlay::dnve3::spatial_density::SpatialDensityUtils;
use crate::overlay::node_store::NodeStore;
use crate::overlay::routing_table::RoutingTable;
use crate::types::NodeId;
use std::sync::{Arc, Mutex};
use web_time::Duration;

#[cfg(test)]
pub(crate) use heartbeat::HeartbeatDensityPayload;

#[derive(Clone)]
pub struct DNVE3Exchanger {
    dnve_data_store: Arc<Mutex<DNVE3DataStore>>,
    node_store: Arc<Mutex<NodeStore>>,
    routing_table: Arc<Mutex<RoutingTable>>,
    local_node_id: NodeId,
    spatial_utils: SpatialDensityUtils,
    aoi_range: f32,
    node_list_aoi_extra_limit: usize,
    hop_count: u32,
}

impl DNVE3Exchanger {
    pub fn new(
        dnve_data_store: Arc<Mutex<DNVE3DataStore>>,
        node_store: Arc<Mutex<NodeStore>>,
        routing_table: Arc<Mutex<RoutingTable>>,
        local_node_id: NodeId,
        config: &Config,
    ) -> Self {
        Self {
            dnve_data_store,
            node_store,
            routing_table,
            local_node_id,
            spatial_utils: SpatialDensityUtils::new_with_partition(
                config.dnve.density_resolution as usize,
                config.dnve.spatial_partition_type,
            ),
            aoi_range: config.dnve.aoi_range,
            node_list_aoi_extra_limit: config.limits.max_connection_count as usize,
            hop_count: config.limits.hop_count,
        }
    }

    pub fn delete_old_data(&self, config: &Config) {
        let mut store = self.dnve_data_store.lock().unwrap();
        let mut node_store = self.node_store.lock().unwrap();

        let now = web_time::Instant::now();
        let expire_duration = Duration::from_secs_f32(config.limits.expire_node_seconds.max(0.0));

        let expired_ids = store
            .density_peers
            .iter()
            .filter(|(_, info)| now.duration_since(info.last_message_time) > expire_duration)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();

        for id in expired_ids {
            store.remove_neighbor(&id);
        }

        node_store.retain_recent(expire_duration);
    }
}
