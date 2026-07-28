pub mod fs;
pub mod meta;
pub mod resolver;

use crate::engine::SessionCtx;
use crate::storage::fs::NativeBlockStore;
use crate::storage::resolver::{NativePeerResolver, TransportSource, WantRegistry};
use async_trait::async_trait;
use mistlib_core::config::StorageConfig;
use mistlib_core::storage::protocol::{build_have_payload, have_chunk_count};
use mistlib_core::storage::{P2PStorage, SelfPositionSource, SpatialPolicy};
use mistlib_core::transport::Transport;
use mistlib_core::types::Vector3;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::sync::OnceCell;

pub type NativeStorageInstance = P2PStorage<NativeBlockStore, NativePeerResolver>;

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
/// a WANT/QUERY broadcast fans out across every active session's transport,
/// snapshotted at request time, since a block's peers may only be reachable
/// through one particular room.
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

    let resolver = NativePeerResolver::new(
        Arc::new(EngineSessionTransports),
        WANT_REGISTRY.clone(),
        5000,
    );
    let max_capacity_bytes = storage_config.max_capacity_mb * 1024 * 1024;
    let storage = P2PStorage::new(
        store,
        resolver,
        max_capacity_bytes,
        Some(Arc::new(EngineSessionPositions) as Arc<dyn SelfPositionSource>),
        SpatialPolicy::from(storage_config),
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
pub async fn handle_want(ctx: Arc<SessionCtx>, from: mistlib_core::types::NodeId, cid: String) {
    if !mistlib_core::storage::is_valid_cid(&cid) {
        tracing::warn!(
            "Storage: dropping WANT with malformed CID (possible path-traversal attempt): {:?}",
            cid
        );
        return;
    }

    if let Some(storage) = STORAGE.get() {
        use mistlib_core::types::DeliveryMethod;

        let block = storage.get_block(&cid).await.ok().flatten();

        if let Some(data) = block {
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
                        tokio::task::yield_now().await;
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
}

/// See `handle_want` for why replies go back via `ctx.transport`.
pub async fn handle_query(ctx: Arc<SessionCtx>, from: mistlib_core::types::NodeId, cid: String) {
    if !mistlib_core::storage::is_valid_cid(&cid) {
        tracing::warn!(
            "Storage: dropping QUERY with malformed CID (possible path-traversal attempt): {:?}",
            cid
        );
        return;
    }

    if let Some(storage) = STORAGE.get() {
        use mistlib_core::types::DeliveryMethod;

        let block_exists = storage.get_block(&cid).await.ok().flatten().is_some();

        if block_exists {
            let msg = resolver::build_have_status_message(&cid);
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
    if !mistlib_core::storage::is_valid_cid(&cid) {
        tracing::warn!(
            "Storage: dropping HAVE with malformed CID (possible path-traversal attempt): {:?}",
            cid
        );
        return;
    }

    WANT_REGISTRY.fulfill(&cid, data);
}

pub fn handle_have_chunk(cid: String, chunk_index: u16, chunk_total: u16, data: Vec<u8>) {
    if !mistlib_core::storage::is_valid_cid(&cid) {
        tracing::warn!(
            "Storage: dropping HAVE_CHUNK with malformed CID (possible path-traversal attempt): {:?}",
            cid
        );
        return;
    }

    WANT_REGISTRY.fulfill_chunk(&cid, chunk_index, chunk_total, data);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_have_payload_uses_single_message() {
        let data = b"small";
        let total = have_chunk_count(data.len()).expect("small block should fit");
        let msg = build_have_payload("cid-small", data, 0, total);
        let parsed = resolver::parse_have_message(&msg).expect("single HAVE should parse");

        assert_eq!(total, 1);
        assert_eq!(parsed.0, "cid-small");
        assert_eq!(parsed.1, data);
    }

    #[test]
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
