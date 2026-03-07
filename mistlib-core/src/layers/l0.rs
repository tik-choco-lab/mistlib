use crate::config::Config;
use crate::stats::EngineStats;
use crate::types::{HostSendSync, NodeId};
use async_trait::async_trait;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait L0Engine: HostSendSync {
    fn initialize(&self, local_id: NodeId, signaling_url: String);
    fn join_room(&self, room_id: String);
    fn leave_room(&self);
    fn set_config(&self, config: Config);
    fn get_config(&self) -> Config;
    async fn get_stats(&self) -> EngineStats;

    async fn storage_add(&self, name: &str, data: &[u8]) -> crate::error::Result<String>;
    async fn storage_get(&self, cid: &str) -> crate::error::Result<Vec<u8>>;
}
