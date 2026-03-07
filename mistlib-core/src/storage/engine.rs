use super::backend::{BlockStore, PeerResolver};
use super::cid::{compute_cid, verify_cid, MULTICODEC_DAG_CBOR, MULTICODEC_RAW};
use super::types::{FileManifest, StorageManager, CHUNK_SIZE};
use crate::error::{MistError, Result};
use std::sync::Mutex;
use tracing::{debug, info, warn};

pub struct StorageEngine<B: BlockStore, P: PeerResolver> {
    store: B,
    resolver: P,
    manager: Mutex<StorageManager>,
}

impl<B: BlockStore, P: PeerResolver> StorageEngine<B, P> {
    pub fn new(store: B, resolver: P, max_capacity_bytes: u64) -> Self {
        Self {
            store,
            resolver,
            manager: Mutex::new(StorageManager::new(max_capacity_bytes)),
        }
    }

    pub async fn add(&self, name: &str, data: &[u8]) -> Result<String> {
        debug!("StorageEngine::add: name={}, size={}", name, data.len());
        let mut chunk_cids = Vec::new();
        for chunk in data.chunks(CHUNK_SIZE) {
            let cid = compute_cid(chunk, MULTICODEC_RAW);
            {
                let mut mgr = self.manager.lock().unwrap();
                mgr.track_block(&cid, chunk.len() as u64);
            }
            self.enforce_capacity_limit().await?;
            self.store.store_block(&cid, chunk).await?;
            chunk_cids.push(cid);
        }

        let manifest = FileManifest {
            name: name.to_string(),
            size: data.len() as u64,
            chunks: chunk_cids,
        };
        let manifest_bytes =
            serde_cbor::to_vec(&manifest).map_err(|e| MistError::Serialization(e.to_string()))?;
        let root_cid = compute_cid(&manifest_bytes, MULTICODEC_DAG_CBOR);

        {
            let mut mgr = self.manager.lock().unwrap();
            mgr.track_block(&root_cid, manifest_bytes.len() as u64);
        }
        self.enforce_capacity_limit().await?;
        self.store.store_block(&root_cid, &manifest_bytes).await?;
        Ok(root_cid)
    }

    pub async fn get(&self, root_cid: &str) -> Result<Vec<u8>> {
        debug!("StorageEngine::get: cid={}", root_cid);
        let manifest_bytes = self.resolve_or_fetch(root_cid, MULTICODEC_DAG_CBOR).await?;
        let manifest: FileManifest = serde_cbor::from_slice(&manifest_bytes)
            .map_err(|e| MistError::Serialization(e.to_string()))?;
        let mut result = Vec::with_capacity(manifest.size as usize);
        for chunk_cid in &manifest.chunks {
            let chunk_data = self.resolve_or_fetch(chunk_cid, MULTICODEC_RAW).await?;
            result.extend_from_slice(&chunk_data);
        }
        Ok(result)
    }

    pub async fn enforce_capacity_limit(&self) -> Result<()> {
        let victims = {
            let mgr = self.manager.lock().unwrap();
            mgr.eviction_candidates()
        };
        for cid in victims {
            info!("StorageEngine: Evicting {}", cid);
            self.store.delete_block(&cid).await?;
            let mut mgr = self.manager.lock().unwrap();
            mgr.untrack_block(&cid);
        }
        Ok(())
    }

    async fn resolve_or_fetch(&self, cid: &str, codec: u64) -> Result<Vec<u8>> {
        if let Some(data) = self.store.load_block(cid).await? {
            self.manager.lock().unwrap().touch(cid);
            return Ok(data);
        }
        let data = self.resolver.resolve_block(cid).await.ok_or_else(|| {
            MistError::Network(format!("Block not found locally or on peers: {}", cid))
        })?;
        if !verify_cid(cid, &data, codec) {
            warn!("StorageEngine: Hash mismatch for {}", cid);
            return Err(MistError::Internal("Hash mismatch".into()));
        }
        {
            let mut mgr = self.manager.lock().unwrap();
            mgr.track_block(cid, data.len() as u64);
        }
        self.enforce_capacity_limit().await?;
        self.store.store_block(cid, &data).await?;
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    struct MemBlockStore {
        blocks: StdMutex<HashMap<String, Vec<u8>>>,
    }

    impl MemBlockStore {
        fn new() -> Self {
            Self {
                blocks: StdMutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl BlockStore for MemBlockStore {
        async fn store_block(&self, cid: &str, data: &[u8]) -> Result<()> {
            self.blocks
                .lock()
                .unwrap()
                .insert(cid.to_string(), data.to_vec());
            Ok(())
        }
        async fn load_block(&self, cid: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.blocks.lock().unwrap().get(cid).cloned())
        }
        async fn delete_block(&self, cid: &str) -> Result<()> {
            self.blocks.lock().unwrap().remove(cid);
            Ok(())
        }
    }

    struct NullResolver;

    #[async_trait]
    impl PeerResolver for NullResolver {
        async fn resolve_block(&self, _cid: &str) -> Option<Vec<u8>> {
            None
        }
    }

    fn block_on<F: std::future::Future<Output = T>, T>(f: F) -> T {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        let mut f = Box::pin(f);
        fn raw_waker_clone() -> RawWaker {
            RawWaker::new(
                std::ptr::null(),
                &RawWakerVTable::new(|_| raw_waker_clone(), |_| {}, |_| {}, |_| {}),
            )
        }
        let waker = unsafe { Waker::from_raw(raw_waker_clone()) };
        let mut cx = Context::from_waker(&waker);
        loop {
            match f.as_mut().poll(&mut cx) {
                Poll::Ready(val) => return val,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn test_add_get_cycle() {
        block_on(async {
            let engine = StorageEngine::new(MemBlockStore::new(), NullResolver, 10 * 1024 * 1024);
            let data = b"Hello modular storage!";
            let root = engine.add("test.txt", data).await.unwrap();
            let retrieved = engine.get(&root).await.unwrap();
            assert_eq!(data.to_vec(), retrieved);
        });
    }

    #[test]
    fn test_multichunk_deduplication() {
        block_on(async {
            let engine = StorageEngine::new(MemBlockStore::new(), NullResolver, 10 * 1024 * 1024);
            let chunk_size = 1024 * 1024;
            let data = vec![0u8; chunk_size * 2];
            let root = engine.add("zeros.bin", &data).await.unwrap();
            let retrieved = engine.get(&root).await.unwrap();
            assert_eq!(data, retrieved);
            
            assert_eq!(engine.manager.lock().unwrap().block_count(), 2);
        });
    }
}
