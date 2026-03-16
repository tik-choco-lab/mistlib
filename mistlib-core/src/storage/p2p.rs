use super::backend::{BlockStore, PeerResolver};
use super::engine::StorageEngine;
use crate::error::Result;
use crate::layers::l2::L2Storage;
use async_trait::async_trait;

pub struct P2PStorage<B: BlockStore, P: PeerResolver> {
    engine: StorageEngine<B, P>,
}

impl<B: BlockStore, P: PeerResolver> P2PStorage<B, P> {
    pub fn new(store: B, resolver: P, max_capacity_bytes: u64) -> Self {
        Self {
            engine: StorageEngine::new(store, resolver, max_capacity_bytes),
        }
    }

    pub async fn get_block(&self, cid: &str) -> Result<Option<Vec<u8>>> {
        self.engine.get_block(cid).await
    }
}

#[cfg(target_arch = "wasm32")]
#[async_trait(?Send)]
impl<B: BlockStore, P: PeerResolver> L2Storage for P2PStorage<B, P> {
    async fn add(&self, name: &str, data: &[u8]) -> Result<String> {
        self.engine.add(name, data).await
    }
    async fn get(&self, root_cid: &str) -> Result<Vec<u8>> {
        self.engine.get(root_cid).await
    }
    async fn enforce_capacity_limit(&self) -> Result<()> {
        self.engine.enforce_capacity_limit().await
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl<B: BlockStore + Send + Sync, P: PeerResolver + Send + Sync> L2Storage for P2PStorage<B, P> {
    async fn add(&self, name: &str, data: &[u8]) -> Result<String> {
        self.engine.add(name, data).await
    }
    async fn get(&self, root_cid: &str) -> Result<Vec<u8>> {
        self.engine.get(root_cid).await
    }
    async fn enforce_capacity_limit(&self) -> Result<()> {
        self.engine.enforce_capacity_limit().await
    }
}
