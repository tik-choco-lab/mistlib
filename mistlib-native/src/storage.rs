pub mod fs;
pub mod meta;
pub mod provider;
pub use provider::{
    fetch_block, register_block_provider, BlockProvider, BlockProviderHandle, FetchOptions,
};
pub mod resolver;

use crate::engine::SessionCtx;
use crate::runtime::TokioRuntime;
use crate::storage::fs::NativeBlockStore;
use crate::storage::resolver::WantRegistry;
use async_trait::async_trait;
use mistlib_core::config::StorageConfig;
use mistlib_core::storage::protocol::{chunk_size_for_limit, have_chunk_count};
use mistlib_core::storage::resolver::{NetworkPeerResolver, ResolverTimings, TransportSource};
use mistlib_core::storage::{P2PStorage, SelfPositionSource, SpatialPolicy};
use mistlib_core::transport::Transport;
use mistlib_core::types::Vector3;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::sync::OnceCell;

pub type NativeStorageInstance = P2PStorage<NativeBlockStore, NetworkPeerResolver>;

pub static STORAGE: OnceCell<Arc<NativeStorageInstance>> = OnceCell::const_new();

/// A process-wide singleton, not a `OnceCell` set inside `init_storage`: two
/// rooms can call `init_storage` concurrently (one per `join_room`), and
/// racing two independent `OnceCell`s (this registry + `STORAGE`) across the
/// `.await` in `NativeBlockStore::new` could install task A's registry with
/// task B's storage -- whose resolver was built from task B's own LOCAL
/// registry. Network handlers below always read this global, so that
/// interleaving would silently strand every `have`/`have_chunk` reply in a
/// registry the installed resolver never reads. `LazyLock` guarantees a
/// single instance regardless of how many callers race to build it.
pub static WANT_REGISTRY: LazyLock<WantRegistry> = LazyLock::new(WantRegistry::new);

/// Feeds the resolver the live set of per-room transports (SPEC-15 rule 8):
/// QUERY broadcasts and targeted WANTs fan out across every active session's
/// transport, snapshotted at request time, since a block's peers may only be
/// reachable through one particular room.
struct EngineSessionTransports;

#[async_trait]
impl TransportSource for EngineSessionTransports {
    async fn transports(&self) -> Vec<Arc<dyn Transport>> {
        crate::engine::ENGINE
            .sessions_snapshot()
            .await
            .into_iter()
            .map(|(_, ctx)| ctx.transport.clone())
            .collect()
    }
    async fn session_transports(&self) -> Vec<(String, Arc<dyn Transport>)> {
        crate::engine::ENGINE
            .sessions_snapshot()
            .await
            .into_iter()
            .map(|(room, ctx)| (room, ctx.transport.clone()))
            .collect()
    }
}

/// Feeds `P2PStorage`'s spatial eviction (SPEC-16) the live self-position of
/// each active session, in join order (mirroring `EngineSessionTransports`
/// above): for each room, the local node's own entry in that room's node
/// store, if one has been recorded (`update_position`/`update_position_in_room`
/// write it there via `NativeL1Transport::update_position`). A room where the
/// local node hasn't reported a position yet is skipped rather than defaulted
/// to `Vector3::zero()` -- core treats an empty result as "no self-position
/// known" and falls back to pure LRU, whereas a fabricated zero would instead
/// make the origin an incorrectly "known" anchor.
struct EngineSessionPositions;

#[async_trait]
impl SelfPositionSource for EngineSessionPositions {
    async fn self_positions(&self) -> Vec<Vector3> {
        let self_id = crate::engine::ENGINE.self_id.lock().unwrap().clone();
        // `sessions_snapshot().await` first, then only synchronous
        // `std::sync::Mutex` locks below -- a guard must never be held
        // across an `.await` point.
        let sessions = crate::engine::ENGINE.sessions_snapshot().await;

        sessions
            .into_iter()
            .filter_map(|(_, ctx)| {
                ctx.node_store
                    .lock()
                    .unwrap()
                    .nodes
                    .get(&self_id)
                    .map(|node| node.position)
            })
            .collect()
    }
}

/// Guards the spatial-decay sweep timer so it is spawned exactly once per
/// process. The primary guarantee comes from gating the sweeper start on
/// winning `STORAGE.set` (a `OnceCell` succeeds exactly once), which also
/// ensures the config that decides whether/how often to sweep is the same
/// config the live storage instance was built from; this flag is cheap
/// defense-in-depth on top of that.
static SWEEPER_STARTED: AtomicBool = AtomicBool::new(false);

pub async fn init_storage(storage_config: &StorageConfig, cache_dir: Option<std::path::PathBuf>) {
    if STORAGE.initialized() {
        return;
    }

    let base_dir = cache_dir.unwrap_or_else(|| std::env::temp_dir().join("mistlib_blocks"));
    // Meta lives in a subdirectory of the block dir; a directory name can
    // never collide with a CID block file, and `NativeBlockStore` refuses
    // non-CID names anyway.
    let meta = crate::storage::meta::NativeMetaStore::new(base_dir.join("meta"))
        .await
        .expect("Failed to init meta store");
    let store = NativeBlockStore::new(base_dir)
        .await
        .expect("Failed to init block store");

    let resolver = NetworkPeerResolver::new_with_transfer(
        Arc::new(EngineSessionTransports) as Arc<dyn TransportSource>,
        Arc::new(TokioRuntime::new(tokio::runtime::Handle::current())),
        WANT_REGISTRY.clone(),
        ResolverTimings::default(),
        storage_config.transfer.clone(),
    );
    let max_capacity_bytes = storage_config.max_capacity_mb * 1024 * 1024;
    let storage = P2PStorage::new_with_transfer(
        store,
        resolver,
        max_capacity_bytes,
        Some(Arc::new(EngineSessionPositions) as Arc<dyn SelfPositionSource>),
        SpatialPolicy::from(storage_config),
        storage_config.transfer.clone(),
    )
    .with_meta_store(Arc::new(meta));

    // Two concurrent joins can both reach this point (the `.await` above is
    // the race window); the second `set` losing is harmless now that both
    // resolvers were built from the SAME `WANT_REGISTRY` singleton, and
    // `create_dir_all` inside `NativeBlockStore::new` is idempotent.
    // Only the caller whose instance actually became live may start the
    // sweeper: otherwise a losing racer's config (e.g. decay enabled) would
    // decide whether the WINNING instance -- possibly built from a config
    // that explicitly opted out -- gets swept.
    if STORAGE.set(Arc::new(storage)).is_ok() {
        maybe_start_decay_sweeper(storage_config);
    }
}

/// Spawns the periodic spatial-decay sweep (SPEC-16) if `spatialDecayEnabled`
/// is set. Core only exposes `run_decay_sweep()` as a one-shot call; driving
/// it on a timer is native/wasm's job. Reads `STORAGE.get()` fresh on every
/// tick rather than capturing the `Arc` built in `init_storage`, so it
/// doesn't matter which racing caller's storage instance actually won
/// `STORAGE.set` above -- the sweep always acts on whichever one is live.
fn maybe_start_decay_sweeper(storage_config: &StorageConfig) {
    if !storage_config.spatial_decay_enabled {
        return;
    }
    if SWEEPER_STARTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let interval_secs = storage_config.spatial_decay_interval_secs.max(1);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            interval.tick().await;
            if let Some(storage) = STORAGE.get() {
                let evicted = storage.run_decay_sweep().await;
                if evicted > 0 {
                    tracing::debug!("Storage: decay sweep evicted {} blocks", evicted);
                }
            }
        }
    });
}

/// Replies go back out via `ctx.transport` -- the session the WANT arrived on
/// (SPEC-15 rule 8) -- rather than any other active room's transport.
struct WantJob {
    ctx: Arc<SessionCtx>,
    from: mistlib_core::types::NodeId,
    cid: String,
}
type WantKey = (usize, mistlib_core::types::NodeId);
static SERVING: std::sync::LazyLock<
    std::sync::Mutex<mistlib_core::storage::responder::WantQueue<WantKey, WantJob>>,
> = std::sync::LazyLock::new(Default::default);

pub(crate) fn enqueue_want(ctx: Arc<SessionCtx>, from: mistlib_core::types::NodeId, cid: String) {
    use mistlib_core::storage::responder::WantAdmission;
    let limited = ctx
        .overlay
        .as_ref()
        .is_some_and(|router| mistlib_core::transport::send::pipeline_enabled(&router.transfer));
    let key = (Arc::as_ptr(&ctx) as usize, from.clone());
    let job = WantJob { ctx, from, cid };
    let first = if limited {
        SERVING.lock().unwrap().enqueue(key.clone(), job)
    } else {
        WantAdmission::Start(job)
    };
    let WantAdmission::Start(mut job) = first else {
        if matches!(first, WantAdmission::Dropped) {
            tracing::debug!(peer = %key.1, "Storage: dropping WANT: 64 pending descriptors");
        }
        return;
    };
    tokio::spawn(async move {
        loop {
            handle_want(job.ctx, job.from, job.cid).await;
            let next = if limited {
                SERVING.lock().unwrap().next(&key)
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

pub async fn handle_want(ctx: Arc<SessionCtx>, from: mistlib_core::types::NodeId, cid: String) {
    tracing::debug!(%cid, peer = %from.0, room = %ctx.room_id, "Storage: WANT received");
    if !mistlib_core::storage::is_valid_cid(&cid) {
        tracing::warn!(
            "Storage: dropping WANT with malformed CID (possible path-traversal attempt): {:?}",
            cid
        );
        return;
    }

    let block = match provider::get_block(&ctx.room_id, &from, &cid).await {
        Some(data) => Some(data),
        None => {
            if let Some(storage) = STORAGE.get() {
                let loaded = storage.get_block(&cid).await;
                tracing::debug!(%cid, peer = %from.0, room = %ctx.room_id, found = loaded.as_ref().is_ok_and(|block| block.is_some()), error = ?loaded.as_ref().err(), "Storage: WANT load result");
                loaded.ok().flatten()
            } else {
                tracing::debug!(%cid, room = %ctx.room_id, "Storage: WANT response ended: storage unavailable");
                return;
            }
        }
    };
    if let Some(data) = block {
        let limit = ctx
            .webrtc_transport
            .as_ref()
            .and_then(|transport| transport.message_size_limit(&from));
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

        tracing::debug!(%cid, peer = %from.0, room = %ctx.room_id, bytes = data.len(), chunk_size, total_chunks, "Storage: WANT response layout");
        if let Err(error) = mistlib_core::storage::responder::send_block(
            ctx.transport.as_ref(),
            &from,
            &cid,
            &data,
            chunk_size,
            total_chunks,
            |index| {
                Box::pin(async move {
                    if total_chunks > 1 && index % 8 == 0 {
                        tokio::task::yield_now().await;
                    }
                })
            },
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
    } else {
        tracing::debug!(%cid, peer = %from.0, room = %ctx.room_id, "Storage: WANT response ended: block unavailable");
    }
}

/// See `handle_want` for why replies go back via `ctx.transport`.
pub async fn handle_query(ctx: Arc<SessionCtx>, from: mistlib_core::types::NodeId, cid: String) {
    tracing::debug!(%cid, peer = %from.0, room = %ctx.room_id, "Storage: QUERY received");
    if !mistlib_core::storage::is_valid_cid(&cid) {
        tracing::warn!(
            "Storage: dropping QUERY with malformed CID (possible path-traversal attempt): {:?}",
            cid
        );
        return;
    }

    let block_exists = if provider::has_block(&ctx.room_id, &from, &cid).await {
        true
    } else if let Some(storage) = STORAGE.get() {
        let loaded = storage.get_block(&cid).await;
        tracing::debug!(%cid, room = %ctx.room_id, error = ?loaded.as_ref().err(), "Storage: QUERY load result");
        loaded.ok().flatten().is_some()
    } else {
        tracing::debug!(%cid, room = %ctx.room_id, "Storage: QUERY response ended: storage unavailable");
        return;
    };

    if block_exists {
        let msg = resolver::build_have_status_message(&cid);
        let result = ctx
            .transport
            .send(
                &from,
                bytes::Bytes::from(msg),
                mistlib_core::types::DeliveryMethod::ReliableOrdered,
            )
            .await;
        tracing::debug!(%cid, peer = %from.0, ?result, "Storage: QUERY response ended");
    } else {
        tracing::debug!(%cid, peer = %from.0, "Storage: QUERY response ended: not held");
    }
}

pub fn handle_have_status(from: mistlib_core::types::NodeId, cid: String) {
    if !mistlib_core::storage::is_valid_cid(&cid) {
        tracing::warn!(
            "Storage: dropping HAVE_STATUS with malformed CID (possible path-traversal attempt): {:?}",
            cid
        );
        return;
    }

    WANT_REGISTRY.register_peer(&cid, from);
}

pub fn handle_have(cid: String, data: Vec<u8>) {
    handle_have_from(mistlib_core::types::NodeId(String::new()), cid, data);
}

pub fn handle_have_from(from: mistlib_core::types::NodeId, cid: String, data: Vec<u8>) {
    if !mistlib_core::storage::is_valid_cid(&cid) {
        tracing::warn!(
            "Storage: dropping HAVE with malformed CID (possible path-traversal attempt): {:?}",
            cid
        );
        return;
    }

    WANT_REGISTRY.fulfill_from(&cid, &from, data);
}

pub fn handle_have_chunk(cid: String, chunk_index: u16, chunk_total: u16, data: Vec<u8>) {
    handle_have_chunk_from(
        mistlib_core::types::NodeId(String::new()),
        cid,
        chunk_index,
        chunk_total,
        data,
    );
}

pub fn handle_have_chunk_from(
    from: mistlib_core::types::NodeId,
    cid: String,
    chunk_index: u16,
    chunk_total: u16,
    data: Vec<u8>,
) {
    if !mistlib_core::storage::is_valid_cid(&cid) {
        tracing::warn!(
            "Storage: dropping HAVE_CHUNK with malformed CID (possible path-traversal attempt): {:?}",
            cid
        );
        return;
    }

    WANT_REGISTRY.fulfill_chunk_from(&cid, &from, chunk_index, chunk_total, data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use mistlib_core::storage::protocol::build_have_payload;

    #[test]
    fn small_have_payload_uses_single_message() {
        let data = b"small";
        let cid = mistlib_core::storage::compute_cid(data, 0x55);
        let total = have_chunk_count(data.len(), resolver::HAVE_CHUNK_SIZE)
            .expect("small block should fit");
        let msg = build_have_payload(&cid, data, 0, total, resolver::HAVE_CHUNK_SIZE);
        let parsed = resolver::parse_have_message(&msg).expect("single HAVE should parse");

        assert_eq!(total, 1);
        assert_eq!(parsed.0, cid);
        assert_eq!(parsed.1, data);
    }

    #[test]
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

    /// Guards the traversal-hardening added to `handle_want`/`handle_query`/
    /// `handle_have*`: those handlers all short-circuit on
    /// `!mistlib_core::storage::is_valid_cid(&cid)` before touching
    /// `STORAGE`/`WANT_REGISTRY`. The handlers themselves aren't driven
    /// directly here because they depend on the process-wide `STORAGE` and
    /// `ENGINE` singletons (a real `NativeBlockStore`, a live session/
    /// transport, etc.) which are impractical to stand up in a unit test.
    /// Instead this asserts the predicate contract the handlers rely on:
    /// every traversal-style payload a WANT/QUERY/HAVE* parser could hand
    /// them is rejected, and a real computed CID is accepted.
    mod malformed_cid_guard_tests {
        use mistlib_core::storage::{compute_cid, is_valid_cid};

        #[test]
        fn rejects_path_traversal_and_absolute_payloads() {
            for cid in [
                "../../etc/passwd",
                "..\\..\\hosts",
                "/etc/passwd",
                "C:\\x",
                "",
            ] {
                assert!(
                    !is_valid_cid(cid),
                    "expected {:?} to be rejected as an invalid CID",
                    cid
                );
            }
        }

        #[test]
        fn accepts_a_real_computed_cid() {
            let cid = compute_cid(b"hi", 0x55);
            assert!(
                is_valid_cid(&cid),
                "a real computed CID must pass the guard used by handle_want/handle_query"
            );
        }
    }

    /// SPEC-16's `EngineSessionPositions`. Reuses the session-registry test
    /// lock/fake-session helpers from `engine::tests` since this also mutates
    /// `ENGINE`'s process-wide session registry and must not race the tests
    /// there.
    mod self_position_source_tests {
        use super::*;
        use crate::engine::tests::session_registry::{fake_session, REGISTRY_TEST_LOCK};
        use crate::engine::ENGINE;
        use mistlib_core::types::NodeId;

        #[tokio::test]
        async fn self_positions_follow_join_order_and_skip_rooms_without_one() {
            let _guard = REGISTRY_TEST_LOCK.lock().await;
            ENGINE.remove_all_sessions().await;

            let original_self_id = ENGINE.self_id.lock().unwrap().clone();
            let self_id = NodeId("self-under-test".to_string());
            *ENGINE.self_id.lock().unwrap() = self_id.clone();

            let room_a = fake_session("room-a");
            room_a
                .node_store
                .lock()
                .unwrap()
                .update_node_position(self_id.clone(), Vector3::new(1.0, 2.0, 3.0));
            ENGINE.insert_session("room-a".to_string(), room_a).await;

            // room-b never records a position for `self_id`: it must be
            // skipped entirely, not reported as `Vector3::zero()`.
            let room_b = fake_session("room-b");
            ENGINE.insert_session("room-b".to_string(), room_b).await;

            let room_c = fake_session("room-c");
            room_c
                .node_store
                .lock()
                .unwrap()
                .update_node_position(self_id.clone(), Vector3::new(4.0, 5.0, 6.0));
            ENGINE.insert_session("room-c".to_string(), room_c).await;

            let positions = EngineSessionPositions.self_positions().await;

            ENGINE.remove_all_sessions().await;
            *ENGINE.self_id.lock().unwrap() = original_self_id;

            assert_eq!(
                positions,
                vec![Vector3::new(1.0, 2.0, 3.0), Vector3::new(4.0, 5.0, 6.0)],
                "join order preserved, room-b (no reported position) skipped"
            );
        }

        #[tokio::test]
        async fn empty_when_no_session_has_reported_a_position() {
            let _guard = REGISTRY_TEST_LOCK.lock().await;
            ENGINE.remove_all_sessions().await;

            ENGINE
                .insert_session("room-a".to_string(), fake_session("room-a"))
                .await;

            let positions = EngineSessionPositions.self_positions().await;

            ENGINE.remove_all_sessions().await;

            assert!(
                positions.is_empty(),
                "no session has recorded self's position yet"
            );
        }
    }
}
