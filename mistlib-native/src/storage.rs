pub mod fs;
pub mod resolver;

use crate::storage::fs::NativeBlockStore;
use crate::storage::resolver::{NativePeerResolver, WantRegistry};
use mistlib_core::storage::P2PStorage;
use std::sync::Arc;
use tokio::sync::OnceCell;

pub type NativeStorageInstance = P2PStorage<NativeBlockStore, NativePeerResolver>;

pub static STORAGE: OnceCell<Arc<NativeStorageInstance>> = OnceCell::const_new();
pub static WANT_REGISTRY: OnceCell<WantRegistry> = OnceCell::const_new();

pub async fn init_storage(
    transport: Arc<dyn mistlib_core::transport::Transport>,
    max_capacity_bytes: u64,
    cache_dir: Option<std::path::PathBuf>,
) {
    let registry = WantRegistry::new();
    WANT_REGISTRY.set(registry.clone()).unwrap_or(());

    let base_dir = cache_dir.unwrap_or_else(|| std::env::temp_dir().join("mistlib_blocks"));
    let store = NativeBlockStore::new(base_dir)
        .await
        .expect("Failed to init block store");

    let resolver = NativePeerResolver::new(transport, registry, 5000);
    let storage = P2PStorage::new(store, resolver, max_capacity_bytes);

    STORAGE.set(Arc::new(storage)).unwrap_or(());
}

pub async fn handle_want(from: mistlib_core::types::NodeId, cid: String) {
    if let Some(storage) = STORAGE.get() {
        use mistlib_core::types::DeliveryMethod;

        let block = storage.get_block(&cid).await.ok().flatten();

        if let Some(data) = block {
            let msg = resolver::build_have_message(&cid, &data);
            if let Some(ctx) = crate::engine::ENGINE.get_context().await {
                let _ = ctx
                    .transport
                    .send(
                        &from,
                        bytes::Bytes::from(msg),
                        DeliveryMethod::ReliableOrdered,
                    )
                    .await;
                tracing::debug!("Storage: served `have` for {} to {}", cid, from.0);
            }
        }
    }
}

pub async fn handle_query(from: mistlib_core::types::NodeId, cid: String) {
    if let Some(storage) = STORAGE.get() {
        use mistlib_core::types::DeliveryMethod;

        let block_exists = storage.get_block(&cid).await.ok().flatten().is_some();

        if block_exists {
            let msg = resolver::build_have_status_message(&cid);
            if let Some(ctx) = crate::engine::ENGINE.get_context().await {
                let _ = ctx
                    .transport
                    .send(
                        &from,
                        bytes::Bytes::from(msg),
                        DeliveryMethod::ReliableOrdered,
                    )
                    .await;
            }
        }
    }
}

pub fn handle_have_status(from: mistlib_core::types::NodeId, cid: String) {
    if let Some(registry) = WANT_REGISTRY.get() {
        registry.register_peer(&cid, from);
    }
}

pub fn handle_have(cid: String, data: Vec<u8>) {
    if let Some(registry) = WANT_REGISTRY.get() {
        registry.fulfill(&cid, data);
    }
}
