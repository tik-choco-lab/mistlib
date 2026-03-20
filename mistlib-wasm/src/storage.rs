pub mod opfs;
pub mod resolver;

use std::cell::RefCell;
use std::sync::Arc;

use js_sys::Uint8Array;
use mistlib_core::layers::l2::L2Storage;
use mistlib_core::storage::P2PStorage;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use opfs::WasmBlockStore;
use resolver::{WantRegistry, WasmPeerResolver};

const DEFAULT_CAPACITY_BYTES: u64 = 512 * 1024 * 1024;

const DEFAULT_PEER_TIMEOUT_MS: u32 = 5_000;

type StorageInstance = P2PStorage<WasmBlockStore, WasmPeerResolver>;

thread_local! {
    static STORAGE: RefCell<Option<StorageInstance>> = RefCell::new(None);
    pub(crate) static WANT_REGISTRY: WantRegistry = WantRegistry::new();
}

pub fn init_storage(
    transport: Arc<dyn mistlib_core::transport::Transport>,
    max_capacity_mb: Option<u64>,
) {
    let capacity = max_capacity_mb.unwrap_or(DEFAULT_CAPACITY_BYTES / (1024 * 1024)) * 1024 * 1024;

    let registry = WANT_REGISTRY.with(|r| r.clone());
    let resolver = WasmPeerResolver::new(transport, registry, DEFAULT_PEER_TIMEOUT_MS);

    let storage = P2PStorage::new(WasmBlockStore, resolver, capacity);

    STORAGE.with(|s| {
        *s.borrow_mut() = Some(storage);
    });

    tracing::info!("Storage: initialized with capacity {} bytes", capacity);
}

pub fn handle_want(from: mistlib_core::types::NodeId, cid: String) {
    spawn_local(async move {
        use mistlib_core::types::DeliveryMethod;

        let data_opt = STORAGE.with(|s| s.borrow().is_some());

        if !data_opt {
            return;
        }

        let block = {
            let store = WasmBlockStore;
            mistlib_core::storage::BlockStore::load_block(&store, &cid)
                .await
                .ok()
                .flatten()
        };

        if let Some(data) = block {
            let ctx = crate::app::ENGINE.with(|e| {
                let state = e.state.lock().unwrap();
                if let mistlib_core::engine::EngineState::Running(ctx) = &*state {
                    Some(ctx.clone())
                } else {
                    None
                }
            });

            if let Some(ctx) = ctx {
                let chunk_size = resolver::HAVE_CHUNK_SIZE;
                let total_chunks = ((data.len() + chunk_size - 1) / chunk_size) as u16;

                if total_chunks <= 1 {
                    let msg = resolver::build_have_message(&cid, &data);
                    let _ = ctx
                        .transport
                        .send(
                            &from,
                            bytes::Bytes::from(msg),
                            DeliveryMethod::ReliableOrdered,
                        )
                        .await;
                } else {
                    for chunk_index in 0..total_chunks {
                        let start = (chunk_index as usize) * chunk_size;
                        let end = ((chunk_index as usize + 1) * chunk_size).min(data.len());
                        let msg = resolver::build_have_chunk_message(
                            &cid,
                            chunk_index,
                            total_chunks,
                            &data[start..end],
                        );

                        let _ = ctx
                            .transport
                            .send(
                                &from,
                                bytes::Bytes::from(msg),
                                DeliveryMethod::ReliableOrdered,
                            )
                            .await;

                        if chunk_index % 8 == 0 {
                            gloo_timers::future::TimeoutFuture::new(0).await;
                        }
                    }
                }

                tracing::debug!(
                    "Storage: served `have` for {} to {} ({} bytes, {} chunks)",
                    cid,
                    from.0,
                    data.len(),
                    total_chunks.max(1)
                );
            }
        }
    });
}

pub fn handle_query(from: mistlib_core::types::NodeId, cid: String) {
    spawn_local(async move {
        use mistlib_core::types::DeliveryMethod;

        let block_exists = {
            let store = WasmBlockStore;
            mistlib_core::storage::BlockStore::load_block(&store, &cid)
                .await
                .ok()
                .flatten()
                .is_some()
        };

        if block_exists {
            let msg = resolver::build_have_status_message(&cid);
            let ctx = crate::app::ENGINE.with(|e| {
                let state = e.state.lock().unwrap();
                if let mistlib_core::engine::EngineState::Running(ctx) = &*state {
                    Some(ctx.clone())
                } else {
                    None
                }
            });

            if let Some(ctx) = ctx {
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
    });
}

pub fn handle_have_status(from: mistlib_core::types::NodeId, cid: String) {
    WANT_REGISTRY.with(|r| r.register_peer(&cid, from));
}

pub fn handle_have(cid: String, data: Vec<u8>) {
    WANT_REGISTRY.with(|r| r.fulfill(&cid, data));
}

#[wasm_bindgen]
pub async fn storage_add(name: String, data: &[u8]) -> Result<String, JsValue> {
    let result = STORAGE.with(|s| {
        if s.borrow().is_none() {
            Err(JsValue::from_str("Storage not initialized"))
        } else {
            Ok(())
        }
    })?;
    let _ = result;

    let root_cid = STORAGE
        .with(|s| -> Result<_, JsValue> {
            let borrow = s.borrow();
            let storage = borrow
                .as_ref()
                .ok_or_else(|| JsValue::from_str("Storage not initialized"))?;

            Ok(storage as *const StorageInstance as usize)
        })
        .and_then(|ptr| {
            let future = unsafe {
                let storage = &*(ptr as *const StorageInstance);
                storage.add(&name, data)
            };
            Ok(future)
        })?
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    Ok(root_cid)
}

#[wasm_bindgen]
pub async fn storage_get(root_cid: String) -> Result<Uint8Array, JsValue> {
    let ptr = STORAGE.with(|s| -> Result<usize, JsValue> {
        let borrow = s.borrow();
        let storage = borrow
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Storage not initialized"))?;
        Ok(storage as *const StorageInstance as usize)
    })?;

    let data = unsafe {
        let storage = &*(ptr as *const StorageInstance);
        storage.get(&root_cid)
    }
    .await
    .map_err(|e| JsValue::from_str(&e.to_string()))?;

    Ok(Uint8Array::from(data.as_slice()))
}
