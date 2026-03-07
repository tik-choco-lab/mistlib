use crate::error::Result;
use crate::types::HostSendSync;
use async_trait::async_trait;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait BlockStore: HostSendSync {
    async fn store_block(&self, cid: &str, data: &[u8]) -> Result<()>;
    async fn load_block(&self, cid: &str) -> Result<Option<Vec<u8>>>;
    async fn delete_block(&self, cid: &str) -> Result<()>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait PeerResolver: HostSendSync {
    async fn resolve_block(&self, cid: &str) -> Option<Vec<u8>>;
}
