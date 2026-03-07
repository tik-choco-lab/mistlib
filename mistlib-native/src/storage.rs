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
    if let Some(_storage) = STORAGE.get() {
        tracing::debug!("handle_want stub for {} from {}", cid, from.0);
    }
}
