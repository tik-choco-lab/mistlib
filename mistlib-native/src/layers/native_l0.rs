mod init;
mod room;
mod storage;

use crate::engine::ENGINE;
use async_trait::async_trait;
use mistlib_core::config::Config;
use mistlib_core::layers::L0Engine;
use mistlib_core::stats::EngineStats;
use mistlib_core::types::NodeId;

pub struct NativeL0;

impl Default for NativeL0 {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeL0 {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl L0Engine for NativeL0 {
    fn initialize(&self, local_id: NodeId, signaling_url: String) {
        init::initialize(local_id, signaling_url);
    }

    fn join_room(&self, room_id: String) {
        room::join_room(room_id);
    }

    fn leave_room(&self) {
        room::leave_room();
    }

    fn set_config(&self, config: Config) {
        let mut cfg = ENGINE.config.lock().unwrap();
        *cfg = config;
    }

    fn get_config(&self) -> Config {
        ENGINE.config.lock().unwrap().clone()
    }

    async fn get_stats(&self) -> EngineStats {
        let stats_str = ENGINE.get_stats_json().await;
        serde_json::from_str(&stats_str).unwrap_or_else(|_| EngineStats {
            message_count: 0,
            send_bits: 0,
            receive_bits: 0,
            rtt_millis: std::collections::HashMap::new(),
            memory_mb: 0.0,
            world_send_bits: 0,
            world_receive_bits: 0,
            world_message_count: 0,
            relay_send_bits: 0,
            relay_receive_bits: 0,
            relay_message_count: 0,
            nodes: vec![],
            diag_peers: 0,
            diag_connection_states: 0,
            diag_pending_candidates: 0,
        })
    }

    async fn storage_add(&self, name: &str, data: &[u8]) -> mistlib_core::error::Result<String> {
        storage::add(name, data).await
    }

    async fn storage_get(&self, cid: &str) -> mistlib_core::error::Result<Vec<u8>> {
        storage::get(cid).await
    }
}
