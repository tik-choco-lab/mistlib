use crate::error::Result;
use crate::types::HostSendSync;
use async_trait::async_trait;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait L2Storage: HostSendSync {
    async fn add(&self, name: &str, data: &[u8]) -> Result<String>;

    async fn get(&self, root_cid: &str) -> Result<Vec<u8>>;

    async fn enforce_capacity_limit(&self) -> Result<()>;
}
