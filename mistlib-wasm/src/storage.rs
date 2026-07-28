pub mod meta;
pub mod opfs;
pub mod position;
pub mod resolver;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use js_sys::Uint8Array;
use mistlib_core::config::StorageConfig;
use mistlib_core::layers::l2::L2Storage;
use mistlib_core::storage::protocol::{build_have_payload, have_chunk_count};
use mistlib_core::storage::{P2PStorage, SpatialPolicy};
use mistlib_core::types::Vector3;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use meta::WasmMetaStore;
use opfs::WasmBlockStore;
use position::WasmSelfPositions;
use resolver::{WantRegistry, WasmPeerResolver};

const DEFAULT_PEER_TIMEOUT_MS: u32 = 5_000;

type StorageInstance = P2PStorage<WasmBlockStore, WasmPeerResolver>;

thread_local! {
    static STORAGE: RefCell<Option<Rc<StorageInstance>>> = RefCell::new(None);
    pub(crate) static WANT_REGISTRY: WantRegistry = WantRegistry::new();
    // Guards against starting the decay-sweep loop twice: `init_storage` can
    // run more than once per process (e.g. a re-init after a full
    // `leave_room()`), but the loop below re-reads `STORAGE` on every tick,
    // so a single loop already tracks whichever storage instance is current
    // -- it never needs to be restarted, only started once.
    static DECAY_SWEEP_STARTED: RefCell<bool> = const { RefCell::new(false) };
}

pub fn init_storage(config: &StorageConfig) {
    let capacity = config.max_capacity_mb * 1024 * 1024;

    let registry = WANT_REGISTRY.with(|r| r.clone());
    let resolver = WasmPeerResolver::new(registry, DEFAULT_PEER_TIMEOUT_MS);

    let storage = P2PStorage::new(
        WasmBlockStore,
        resolver,
        capacity,
        Some(Arc::new(WasmSelfPositions)),
        SpatialPolicy::from(config),
    )
    .with_meta_store(Arc::new(WasmMetaStore));

    STORAGE.with(|s| {
        *s.borrow_mut() = Some(Rc::new(storage));
    });

    tracing::info!("Storage: initialized with capacity {} bytes", capacity);

    start_decay_sweep(config);
}

/// Drives SPEC-16's periodic decay sweep on a `spatial_decay_interval_secs`
/// timer, mirroring the `spawn_local` + `loop` + `TimeoutFuture` pattern used
/// by `signaling::nostr::refresh::spawn_discovery_refresh`. A no-op if decay
/// isn't enabled or the loop is already running (see `DECAY_SWEEP_STARTED`).
fn start_decay_sweep(config: &StorageConfig) {
    if !config.spatial_decay_enabled {
        return;
    }

    let already_started = DECAY_SWEEP_STARTED.with(|f| f.replace(true));
    if already_started {
        return;
    }

    let interval_ms = config
        .spatial_decay_interval_secs
        .saturating_mul(1000)
        .max(1)
        .min(u64::from(u32::MAX)) as u32;

    spawn_local(async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(interval_ms).await;

            let storage = STORAGE.with(|s| s.borrow().clone());
            if let Some(storage) = storage {
                let evicted = storage.run_decay_sweep().await;
                if evicted > 0 {
                    tracing::debug!("Storage: decay sweep evicted {} block(s)", evicted);
                }
            }
        }
    });
}

/// `room_id` is the session the WANT arrived on -- storage itself is
/// process-wide (rule 10), but the reply must go back through that specific
/// session's transport, which is the only one guaranteed to have a route to
/// `from`.
pub fn handle_want(room_id: String, from: mistlib_core::types::NodeId, cid: String) {
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
            let ctx = crate::app::session_running_ctx(&room_id);

            if let Some(ctx) = ctx {
                let Some(total_chunks) = have_chunk_count(data.len()) else {
                    tracing::warn!(
                        "Storage: refusing to serve oversized block {} ({} bytes, {} chunks)",
                        cid,
                        data.len(),
                        data.len().div_ceil(resolver::HAVE_CHUNK_SIZE)
                    );
                    return;
                };

                if total_chunks <= 1 {
                    let msg = build_have_payload(&cid, &data, 0, total_chunks);
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
                        let msg = build_have_payload(&cid, &data, chunk_index, total_chunks);

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

pub fn handle_query(room_id: String, from: mistlib_core::types::NodeId, cid: String) {
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
            let ctx = crate::app::session_running_ctx(&room_id);

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
    let storage = STORAGE.with(|s| -> Result<Rc<StorageInstance>, JsValue> {
        s.borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| JsValue::from_str("Storage not initialized"))
    })?;
    let data = data.to_vec();

    let root_cid = storage
        .add(&name, &data)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    Ok(root_cid)
}

/// Explicit-position variant of `storage_add` (SPEC-16): every chunk and the
/// manifest are tagged with `(x, y, z)` instead of being auto-tagged from
/// `WasmSelfPositions`.
#[wasm_bindgen]
pub async fn storage_add_at(
    name: String,
    data: &[u8],
    x: f32,
    y: f32,
    z: f32,
) -> Result<String, JsValue> {
    let storage = STORAGE.with(|s| -> Result<Rc<StorageInstance>, JsValue> {
        s.borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| JsValue::from_str("Storage not initialized"))
    })?;
    let data = data.to_vec();

    let root_cid = storage
        .add_at(&name, &data, Some(Vector3::new(x, y, z)))
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    Ok(root_cid)
}

#[wasm_bindgen]
pub async fn storage_get(root_cid: String) -> Result<Uint8Array, JsValue> {
    let storage = STORAGE.with(|s| -> Result<Rc<StorageInstance>, JsValue> {
        s.borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| JsValue::from_str("Storage not initialized"))
    })?;

    let data = storage
        .get(&root_cid)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    Ok(Uint8Array::from(data.as_slice()))
}

/// Pins `root_cid` so it (and every chunk its manifest references) is
/// exempt from eviction/decay (SPEC-18). See `StorageEngine::pin`.
#[wasm_bindgen]
pub async fn storage_pin(root_cid: String) -> Result<(), JsValue> {
    let storage = STORAGE.with(|s| -> Result<Rc<StorageInstance>, JsValue> {
        s.borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| JsValue::from_str("Storage not initialized"))
    })?;

    storage
        .pin(&root_cid)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Removes `root_cid`'s pin (SPEC-18). See `StorageEngine::unpin`.
#[wasm_bindgen]
pub async fn storage_unpin(root_cid: String) -> Result<(), JsValue> {
    let storage = STORAGE.with(|s| -> Result<Rc<StorageInstance>, JsValue> {
        s.borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| JsValue::from_str("Storage not initialized"))
    })?;

    storage
        .unpin(&root_cid)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Whether `root_cid` is currently pinned (SPEC-18). See
/// `StorageEngine::is_pinned`.
#[wasm_bindgen]
pub async fn storage_is_pinned(root_cid: String) -> Result<bool, JsValue> {
    let storage = STORAGE.with(|s| -> Result<Rc<StorageInstance>, JsValue> {
        s.borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| JsValue::from_str("Storage not initialized"))
    })?;

    Ok(storage.is_pinned(&root_cid).await)
}

/// `storage_add` + `storage_pin` with no gap between them for a concurrent
/// eviction to exploit (SPEC-18). See `StorageEngine::add_pinned`.
#[wasm_bindgen]
pub async fn storage_add_pinned(name: String, data: &[u8]) -> Result<String, JsValue> {
    let storage = STORAGE.with(|s| -> Result<Rc<StorageInstance>, JsValue> {
        s.borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| JsValue::from_str("Storage not initialized"))
    })?;
    let data = data.to_vec();

    storage
        .add_pinned(&name, &data)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn one_mib_have_payload_is_split_into_datachannel_safe_chunks() {
        let data = vec![7u8; 1024 * 1024];
        let total = have_chunk_count(data.len()).expect("1MiB block should fit");

        assert_eq!(
            total as usize,
            data.len().div_ceil(resolver::HAVE_CHUNK_SIZE)
        );
        assert!(total > 1, "1MiB block must not be sent as one HAVE frame");

        let mut reassembled = Vec::with_capacity(data.len());
        for chunk_index in 0..total {
            let msg = build_have_payload("cid-large", &data, chunk_index, total);
            assert_ne!(msg[0], resolver::MSG_HAVE);

            let (cid, parsed_index, parsed_total, payload) =
                resolver::parse_have_chunk_message(&msg).expect("chunk HAVE should parse");
            assert_eq!(cid, "cid-large");
            assert_eq!(parsed_index, chunk_index);
            assert_eq!(parsed_total, total);
            assert!(payload.len() <= resolver::HAVE_CHUNK_SIZE);
            reassembled.extend_from_slice(&payload);
        }

        assert_eq!(reassembled, data);
    }

    /// Before this commit, wasm's `handle_want` computed `total_chunks` as
    /// `((data.len() + chunk_size - 1) / chunk_size) as u16` inline: for a
    /// block needing exactly `u16::MAX + 1` chunks, that cast silently wraps
    /// to `0` instead of erroring, corrupting the chunk count sent to peers.
    /// `have_chunk_count` -- the guard native already had, now shared via
    /// mistlib-core -- must refuse (`None`) instead, matching native exactly
    /// since both crates call the same function.
    #[wasm_bindgen_test]
    fn oversized_block_is_refused_rather_than_wrapped() {
        let oversized_len = (u16::MAX as usize + 1) * resolver::HAVE_CHUNK_SIZE;
        assert!(
            have_chunk_count(oversized_len).is_none(),
            "oversized block must be refused, not silently wrapped"
        );
    }
}
