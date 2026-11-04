mod cooperative;
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
use mistlib_core::storage::protocol::{chunk_size_for_limit, have_chunk_count};
use mistlib_core::storage::{P2PStorage, SpatialPolicy};
use mistlib_core::types::Vector3;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use meta::WasmMetaStore;
use opfs::WasmBlockStore;
use position::WasmSelfPositions;
use resolver::{WantRegistry, WasmPeerResolver};

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
    let resolver = WasmPeerResolver::new(registry);

    let storage = P2PStorage::new_with_transfer(
        WasmBlockStore,
        resolver,
        capacity,
        Some(Arc::new(WasmSelfPositions)),
        SpatialPolicy::from(config),
        config.transfer.clone(),
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
type WantJob = (
    Arc<mistlib_core::engine::RunningContext>,
    String,
    mistlib_core::types::NodeId,
    String,
);
type WantKey = (usize, mistlib_core::types::NodeId);
thread_local! {
    static SERVING: RefCell<mistlib_core::storage::responder::WantQueue<WantKey, WantJob>> = RefCell::new(Default::default());
}

pub fn handle_want(room_id: String, from: mistlib_core::types::NodeId, cid: String) {
    use mistlib_core::storage::responder::WantAdmission;
    let Some(ctx) = crate::app::session_running_ctx(&room_id) else {
        return;
    };
    let limited = ctx
        .overlay
        .as_ref()
        .is_some_and(|router| mistlib_core::transport::send::pipeline_enabled(&router.transfer));
    let key = (Arc::as_ptr(&ctx) as usize, from.clone());
    let job = (ctx, room_id, from, cid);
    let first = if limited {
        SERVING.with(|q| q.borrow_mut().enqueue(key.clone(), job))
    } else {
        WantAdmission::Start(job)
    };
    let WantAdmission::Start(mut job) = first else {
        if matches!(first, WantAdmission::Dropped) {
            tracing::debug!(peer = %key.1, "Storage: dropping WANT: 64 pending descriptors");
        }
        return;
    };
    spawn_local(async move {
        loop {
            serve_want(job.0, job.1, job.2, job.3).await;
            let next = if limited {
                SERVING.with(|q| q.borrow_mut().next(&key))
            } else {
                None
            };
            match next {
                Some(next) => job = next,
                None => break,
            }
        }
    });
}

async fn serve_want(
    ctx: Arc<mistlib_core::engine::RunningContext>,
    room_id: String,
    from: mistlib_core::types::NodeId,
    cid: String,
) {
    tracing::debug!(%cid, peer = %from.0, room = %room_id, "Storage: WANT received");

    let data_opt = STORAGE.with(|s| s.borrow().is_some());

    if !data_opt {
        tracing::debug!(%cid, room = %room_id, "Storage: WANT response ended: storage unavailable");
        return;
    }

    let block = {
        let store = WasmBlockStore;
        let loaded = mistlib_core::storage::BlockStore::load_block(&store, &cid).await;
        tracing::debug!(%cid, peer = %from.0, room = %room_id, found = loaded.as_ref().is_ok_and(|block| block.is_some()), error = ?loaded.as_ref().err(), "Storage: WANT load result");
        loaded.ok().flatten()
    };

    if let Some(data) = block {
        {
            let limit = ctx.preferred_transport().message_size_limit(&from);
            let chunk_size = chunk_size_for_limit(&cid, limit);
            let Some(total_chunks) = have_chunk_count(data.len(), chunk_size) else {
                tracing::warn!(
                    "Storage: refusing to serve oversized block {} ({} bytes, {} chunks)",
                    cid,
                    data.len(),
                    data.len().div_ceil(chunk_size)
                );
                return;
            };

            tracing::debug!(%cid, peer = %from.0, room = %room_id, bytes = data.len(), chunk_size, total_chunks, "Storage: WANT response layout");
            if let Err(error) = mistlib_core::storage::responder::send_block(
                ctx.transport.as_ref(),
                &crate::runtime::WasmRuntime,
                &from,
                &cid,
                &data,
                chunk_size,
                total_chunks,
                |_| Box::pin(async {}),
            )
            .await
            {
                tracing::debug!(%cid, peer = %from, %error, "Storage: WANT response stopped after send failure");
                return;
            }

            tracing::debug!(
                "Storage: WANT response ended for {} to {} ({} bytes, {} chunks queued)",
                cid,
                from.0,
                data.len(),
                total_chunks.max(1)
            );
        }
    } else {
        tracing::debug!(%cid, room = %room_id, "Storage: WANT response ended: block unavailable");
    }
}

pub fn handle_query(room_id: String, from: mistlib_core::types::NodeId, cid: String) {
    tracing::debug!(%cid, peer = %from.0, room = %room_id, "Storage: QUERY received");
    spawn_local(async move {
        use mistlib_core::types::DeliveryMethod;

        let block_exists = {
            let store = WasmBlockStore;
            let loaded = mistlib_core::storage::BlockStore::load_block(&store, &cid).await;
            tracing::debug!(%cid, peer = %from.0, room = %room_id, found = loaded.as_ref().is_ok_and(|block| block.is_some()), error = ?loaded.as_ref().err(), "Storage: QUERY load result");
            loaded.ok().flatten().is_some()
        };

        if block_exists {
            let msg = resolver::build_have_status_message(&cid);
            let ctx = crate::app::session_running_ctx(&room_id);

            if let Some(ctx) = ctx {
                let result = ctx
                    .transport
                    .send(
                        &from,
                        bytes::Bytes::from(msg),
                        DeliveryMethod::ReliableOrdered,
                    )
                    .await;
                tracing::debug!(%cid, peer = %from.0, room = %room_id, ?result, "Storage: QUERY response ended");
            } else {
                tracing::debug!(%cid, room = %room_id, "Storage: QUERY response ended: session unavailable");
            }
        } else {
            tracing::debug!(%cid, room = %room_id, "Storage: QUERY response ended: block unavailable");
        }
    });
}

pub fn handle_have_status(from: mistlib_core::types::NodeId, cid: String) {
    WANT_REGISTRY.with(|r| r.register_peer(&cid, from));
}

pub fn handle_have(cid: String, data: Vec<u8>) {
    handle_have_from(mistlib_core::types::NodeId(String::new()), cid, data);
}

pub fn handle_have_from(from: mistlib_core::types::NodeId, cid: String, data: Vec<u8>) {
    WANT_REGISTRY.with(|r| r.fulfill_from(&cid, &from, data));
}

enum AddMode {
    Unpinned,
    Pinned,
    At(Vector3),
}

fn add_from_js(name: String, data: Uint8Array, mode: AddMode) -> js_sys::Promise {
    // Preserve the existing snapshot-at-call contract even if the caller
    // changes its buffer while the async copy/storage operation is pending.
    let snapshot = data.slice(0, data.length());
    add_snapshot(name, snapshot, mode)
}

#[wasm_bindgen(module = "/src/storage/owned.js")]
extern "C" {
    #[wasm_bindgen(catch, js_name = takeStorageInput)]
    fn take_storage_input(data: &Uint8Array) -> Result<Uint8Array, JsValue>;
}

fn add_owned_from_js(name: String, data: Uint8Array, mode: AddMode) -> js_sys::Promise {
    if STORAGE.with(|s| s.borrow().is_none()) {
        return js_sys::Promise::reject(&JsValue::from_str("Storage not initialized"));
    }
    match take_storage_input(&data) {
        Ok(owned) => add_snapshot(name, owned, mode),
        Err(error) => js_sys::Promise::reject(&error),
    }
}

fn add_snapshot(name: String, snapshot: Uint8Array, mode: AddMode) -> js_sys::Promise {
    wasm_bindgen_futures::future_to_promise(async move {
        // Capture this storage instance before yielding, as the original async
        // entry points do, even if the application reinitializes meanwhile.
        let storage = STORAGE.with(|s| {
            s.borrow()
                .as_ref()
                .cloned()
                .ok_or_else(|| JsValue::from_str("Storage not initialized"))
        })?;
        let data = cooperative::from_js(&snapshot).await?;
        let cid = match mode {
            AddMode::Unpinned => storage.add(&name, &data).await,
            AddMode::Pinned => storage.add_pinned(&name, &data).await,
            AddMode::At(position) => storage.add_at(&name, &data, Some(position)).await,
        }
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(JsValue::from_str(&cid))
    })
}

#[wasm_bindgen(js_name = storage_add, unchecked_return_type = "Promise<string>")]
pub fn storage_add_js(name: String, data: Uint8Array) -> js_sys::Promise {
    add_from_js(name, data, AddMode::Unpinned)
}

#[wasm_bindgen(js_name = storage_add_at, unchecked_return_type = "Promise<string>")]
pub fn storage_add_at_js(
    name: String,
    data: Uint8Array,
    x: f32,
    y: f32,
    z: f32,
) -> js_sys::Promise {
    add_from_js(name, data, AddMode::At(Vector3::new(x, y, z)))
}

#[wasm_bindgen(js_name = storage_add_pinned, unchecked_return_type = "Promise<string>")]
pub fn storage_add_pinned_js(name: String, data: Uint8Array) -> js_sys::Promise {
    add_from_js(name, data, AddMode::Pinned)
}

/// Takes ownership of data's entire backing ArrayBuffer synchronously, detaching
/// every alias. Only the view's bytes are stored. Rejects if transfer is unavailable
/// or the buffer is shared, detached or non-transferable; never falls back to a copy.
/// Once accepted, ownership is consumed even if the asynchronous storage write fails.
#[wasm_bindgen(unchecked_return_type = "Promise<string>")]
pub fn storage_add_owned(name: String, data: Uint8Array) -> js_sys::Promise {
    add_owned_from_js(name, data, AddMode::Unpinned)
}

/// Ownership-taking variant of storage_add_pinned, with the same atomic pinning.
/// Detaches the entire backing buffer before returning; see storage_add_owned.
#[wasm_bindgen(unchecked_return_type = "Promise<string>")]
pub fn storage_add_pinned_owned(name: String, data: Uint8Array) -> js_sys::Promise {
    add_owned_from_js(name, data, AddMode::Pinned)
}

pub async fn storage_add(name: String, data: &[u8]) -> Result<String, JsValue> {
    let storage = STORAGE.with(|s| -> Result<Rc<StorageInstance>, JsValue> {
        s.borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| JsValue::from_str("Storage not initialized"))
    })?;
    let root_cid = storage
        .add(&name, data)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    Ok(root_cid)
}

/// Explicit-position variant of `storage_add` (SPEC-16): every chunk and the
/// manifest are tagged with `(x, y, z)` instead of being auto-tagged from
/// `WasmSelfPositions`.
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
    let root_cid = storage
        .add_at(&name, data, Some(Vector3::new(x, y, z)))
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

    cooperative::to_js(&data).await
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
pub async fn storage_add_pinned(name: String, data: &[u8]) -> Result<String, JsValue> {
    let storage = STORAGE.with(|s| -> Result<Rc<StorageInstance>, JsValue> {
        s.borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| JsValue::from_str("Storage not initialized"))
    })?;
    storage
        .add_pinned(&name, data)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mistlib_core::storage::protocol::build_have_payload;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    async fn digest_worker_routes_parallel_results_and_restarts_after_idle() {
        use mistlib_core::storage::{cid::MULTICODEC_RAW, BlockStore};
        futures::future::join_all((1..=4).map(|i| async move {
            let data = vec![i as u8; i * 4096];
            assert_eq!(
                opfs::WasmBlockStore
                    .compute_cid(&data, MULTICODEC_RAW)
                    .await
                    .unwrap(),
                mistlib_core::storage::compute_cid(&data, MULTICODEC_RAW)
            );
        }))
        .await;
        gloo_timers::future::TimeoutFuture::new(1100).await;
        let data = vec![9; 4096];
        assert_eq!(
            opfs::WasmBlockStore
                .compute_cid(&data, MULTICODEC_RAW)
                .await
                .unwrap(),
            mistlib_core::storage::compute_cid(&data, MULTICODEC_RAW)
        );
    }

    #[wasm_bindgen_test]
    async fn resolver_budget_defers_polls_beyond_microtasks_and_cancels() {
        use std::cell::Cell;
        use std::task::Poll;
        let budget = cooperative::PollBudget::new(web_time::Duration::ZERO);
        let polls = Cell::new(0);
        let make_work = || {
            futures::future::poll_fn(|cx| {
                polls.set(polls.get() + 1);
                if polls.get() % 2 == 1 {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(42)
                }
            })
        };
        let mut a = Box::pin(budget.run(make_work()));
        let mut b = Box::pin(budget.run(make_work()));
        assert!(futures::poll!(&mut a).is_pending());
        assert!(futures::poll!(&mut b).is_pending());
        wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(&JsValue::NULL))
            .await
            .unwrap();
        assert_eq!(polls.get(), 0, "a microtask must not resume resolver work");
        assert_eq!(futures::future::join(a, b).await, (42, 42));
        assert_eq!(polls.get(), 4);

        let cancelled = Cell::new(false);
        let mut work = Box::pin(budget.run(async {
            cancelled.set(true);
        }));
        assert!(futures::poll!(&mut work).is_pending());
        drop(work);
        gloo_timers::future::TimeoutFuture::new(10).await;
        assert!(!cancelled.get());
    }

    #[wasm_bindgen_test]
    async fn owned_add_detaches_aliases_preserves_view_and_pin() {
        init_storage(&StorageConfig {
            spatial_decay_enabled: false,
            ..StorageConfig::default()
        });
        for pinned in [false, true] {
            for size in [0, 1024 * 1024 + 1] {
                let bytes: Vec<u8> = (0..size + 17).map(|i| (i * 31) as u8).collect();
                let buffer = Uint8Array::from(bytes.as_slice());
                let view = buffer.subarray(7, (7 + size) as u32);
                let name = format!("owned-{pinned}-{size}.bin");
                let expected = storage_add(name.clone(), &bytes[7..7 + size])
                    .await
                    .unwrap();
                let promise = if pinned {
                    storage_add_pinned_owned(name, view.clone())
                } else {
                    storage_add_owned(name, view.clone())
                };
                assert_eq!(buffer.buffer().byte_length(), 0);
                assert_eq!(buffer.length(), 0);
                assert_eq!(view.length(), 0);
                let cid = wasm_bindgen_futures::JsFuture::from(promise)
                    .await
                    .unwrap()
                    .as_string()
                    .unwrap();
                assert_eq!(cid, expected);
                assert_eq!(
                    storage_get(cid.clone()).await.unwrap().to_vec(),
                    bytes[7..7 + size]
                );
                assert_eq!(storage_is_pinned(cid).await.unwrap(), pinned);
            }
        }
    }

    #[wasm_bindgen_test]
    async fn owned_add_rejects_detached_and_wasm_memory_inputs() {
        init_storage(&StorageConfig {
            spatial_decay_enabled: false,
            ..StorageConfig::default()
        });
        let detached = Uint8Array::new_with_length(1);
        take_storage_input(&detached).unwrap();
        let memory = wasm_bindgen::memory().unchecked_into::<js_sys::WebAssembly::Memory>();
        let wasm_view = Uint8Array::new(&memory.buffer()).subarray(0, 1);
        for input in [detached, wasm_view] {
            let before = input.buffer().byte_length();
            assert!(wasm_bindgen_futures::JsFuture::from(storage_add_owned(
                "reject.bin".into(),
                input.clone()
            ))
            .await
            .is_err());
            assert_eq!(input.buffer().byte_length(), before);
        }
    }

    #[wasm_bindgen_test]
    async fn js_add_snapshots_input_before_returning_its_promise() {
        init_storage(&StorageConfig {
            spatial_decay_enabled: false,
            ..StorageConfig::default()
        });
        let expected: Vec<u8> = (0..1024 * 1024 + 1).map(|i| (i * 31) as u8).collect();
        for mode in 0..3 {
            let input = Uint8Array::from(expected.as_slice());
            let name = format!("snapshot-{mode}.bin");
            let promise = match mode {
                0 => storage_add_js(name, input.clone()),
                1 => storage_add_pinned_js(name, input.clone()),
                _ => storage_add_at_js(name, input.clone(), 1.0, 2.0, 3.0),
            };
            input.fill(0, 0, input.length());
            let cid = wasm_bindgen_futures::JsFuture::from(promise)
                .await
                .unwrap()
                .as_string()
                .unwrap();
            assert_eq!(storage_get(cid.clone()).await.unwrap().to_vec(), expected);
            assert_eq!(storage_is_pinned(cid).await.unwrap(), mode == 1);
        }
    }

    #[wasm_bindgen_test]
    async fn cooperative_copy_roundtrips_boundaries() {
        for size in [
            0,
            1,
            256 * 1024 - 1,
            256 * 1024,
            256 * 1024 + 1,
            4 * 1024 * 1024,
        ] {
            let expected: Vec<u8> = (0..size).map(|i| (i * 13 + i / 256) as u8).collect();
            let js = cooperative::to_js(&expected).await.unwrap();
            assert_eq!(cooperative::from_js(&js).await.unwrap(), expected);
        }
    }

    #[wasm_bindgen_test]
    async fn browser_digest_preserves_storage_cids_and_detects_changes() {
        use mistlib_core::storage::{cid::MULTICODEC_RAW, BlockStore};
        for size in [0, 4095, 4096, 1024 * 1024] {
            let data: Vec<u8> = (0..size).map(|i| (i * 17 + i / 256) as u8).collect();
            for codec in [MULTICODEC_RAW, 0x71] {
                let cid = opfs::WasmBlockStore
                    .compute_cid(&data, codec)
                    .await
                    .unwrap();
                assert_eq!(cid, mistlib_core::storage::compute_cid(&data, codec));
                if size > 0 {
                    let mut changed = data.clone();
                    changed[size - 1] ^= 1;
                    assert_ne!(
                        cid,
                        opfs::WasmBlockStore
                            .compute_cid(&changed, codec)
                            .await
                            .unwrap()
                    );
                }
            }
        }
    }

    #[wasm_bindgen_test]
    fn one_mib_have_payload_is_split_into_datachannel_safe_chunks() {
        let data = vec![7u8; 1024 * 1024];
        let expected_cid = mistlib_core::storage::compute_cid(&data, 0x55);
        let total =
            have_chunk_count(data.len(), resolver::HAVE_CHUNK_SIZE).expect("1MiB block should fit");

        assert_eq!(
            total as usize,
            data.len().div_ceil(resolver::HAVE_CHUNK_SIZE)
        );
        assert!(total > 1, "1MiB block must not be sent as one HAVE frame");

        let mut reassembled = Vec::with_capacity(data.len());
        for chunk_index in 0..total {
            let msg = build_have_payload(
                &expected_cid,
                &data,
                chunk_index,
                total,
                resolver::HAVE_CHUNK_SIZE,
            );
            assert_ne!(msg[0], resolver::MSG_HAVE);

            let (cid, parsed_index, parsed_total, payload) =
                resolver::parse_have_chunk_message(&msg).expect("chunk HAVE should parse");
            assert_eq!(cid, expected_cid);
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
            have_chunk_count(oversized_len, resolver::HAVE_CHUNK_SIZE).is_none(),
            "oversized block must be refused, not silently wrapped"
        );
    }
}
