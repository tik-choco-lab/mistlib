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

/// Like `MemBlockStore`, but appends a `"store:<cid>"`/`"delete:<cid>"` entry
/// to a shared log on every call, so tests can assert about *when* an
/// eviction happened relative to other block writes (not just the end state).
struct LoggingBlockStore {
    blocks: StdMutex<HashMap<String, Vec<u8>>>,
    log: StdMutex<Vec<String>>,
}

impl LoggingBlockStore {
    fn new() -> Self {
        Self {
            blocks: StdMutex::new(HashMap::new()),
            log: StdMutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl BlockStore for LoggingBlockStore {
    async fn store_block(&self, cid: &str, data: &[u8]) -> Result<()> {
        self.log.lock().unwrap().push(format!("store:{cid}"));
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
        self.log.lock().unwrap().push(format!("delete:{cid}"));
        self.blocks.lock().unwrap().remove(cid);
        Ok(())
    }
}

/// In-memory `MetaStore` for tests (SPEC-17/SPEC-18): same flat key->bytes
/// contract as the real OPFS/native backends, scoped to the test's lifetime.
struct MemMetaStore {
    entries: StdMutex<HashMap<String, Vec<u8>>>,
}

impl MemMetaStore {
    fn new() -> Self {
        Self {
            entries: StdMutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl MetaStore for MemMetaStore {
    async fn set(&self, key: &str, data: &[u8]) -> Result<()> {
        self.entries
            .lock()
            .unwrap()
            .insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.entries.lock().unwrap().get(key).cloned())
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.entries.lock().unwrap().remove(key);
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

/// A future that returns `Pending` exactly once, allowing sibling futures
/// driven by the same `FuturesUnordered` to make progress before this one
/// resumes. Used to deterministically expose in-flight overlap without
/// relying on wall-clock timers (which are unavailable under the busy-loop
/// `block_on` and would be flaky).
struct YieldOnce(bool);

impl std::future::Future for YieldOnce {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.0 {
            std::task::Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

/// A `PeerResolver` that serves blocks from a seeded map while recording the
/// maximum number of resolves that were ever in flight simultaneously.
///
/// In a client/server model each block is fetched in a single serialized
/// request stream, so concurrency would stay at 1. The P2P engine fans the
/// chunk requests out across peers in parallel, so the observed peak should
/// rise to the engine's concurrency window.
struct CountingResolver {
    blocks: HashMap<String, Vec<u8>>,
    active: StdMutex<usize>,
    max_active: StdMutex<usize>,
}

impl CountingResolver {
    fn new(blocks: HashMap<String, Vec<u8>>) -> Self {
        Self {
            blocks,
            active: StdMutex::new(0),
            max_active: StdMutex::new(0),
        }
    }
}

#[async_trait]
impl PeerResolver for CountingResolver {
    async fn resolve_block(&self, cid: &str) -> Option<Vec<u8>> {
        {
            let mut active = self.active.lock().unwrap();
            *active += 1;
            let mut max = self.max_active.lock().unwrap();
            if *active > *max {
                *max = *active;
            }
        }
        // Yield repeatedly so any concurrently-issued sibling resolves get a
        // chance to enter this critical section before we finish.
        for _ in 0..16 {
            YieldOnce(false).await;
        }
        let data = self.blocks.get(cid).cloned();
        *self.active.lock().unwrap() -= 1;
        data
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
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        );
        let data = b"Hello modular storage!";
        let root = engine.add("test.txt", data).await.unwrap();
        let retrieved = engine.get(&root).await.unwrap();
        assert_eq!(data.to_vec(), retrieved);
    });
}

#[test]
fn test_multichunk_deduplication() {
    block_on(async {
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        );
        let chunk_size = 1024 * 1024;
        let data = vec![0u8; chunk_size * 2];
        let root = engine.add("zeros.bin", &data).await.unwrap();
        let retrieved = engine.get(&root).await.unwrap();
        assert_eq!(data, retrieved);

        assert_eq!(engine.manager.lock().unwrap().block_count(), 2);
    });
}

#[test]
fn test_blocks_are_fetched_from_peers_in_parallel() {
    block_on(async {
        // Seed an engine and capture every block it produced (manifest + chunks).
        let seeder = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            64 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        );
        let chunk_count = 8usize;
        let data = vec![7u8; CHUNK_SIZE * chunk_count];
        let root = seeder.add("parallel.bin", &data).await.unwrap();
        let seeded: HashMap<String, Vec<u8>> = seeder.store.blocks.lock().unwrap().clone();

        // A fresh engine with an empty store must pull everything from peers.
        let resolver = CountingResolver::new(seeded);
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            resolver,
            64 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        );
        let retrieved = engine.get(&root).await.unwrap();

        // Correctness: parallel fetch + reordering still yields the exact bytes.
        assert_eq!(data, retrieved);

        // Efficiency: chunk fetches overlapped instead of running one-at-a-time
        // (the client/server baseline). The engine caps concurrency at 4, and
        // with 8 chunks the window should fill completely.
        let peak = *engine.resolver.max_active.lock().unwrap();
        assert!(
            peak >= 4,
            "expected parallel peer fetches (peak >= 4), observed peak {peak}"
        );
        assert!(
            peak > 1,
            "fetches ran sequentially like client/server, peak {peak}"
        );
    });
}

/// A resolver simulating several seeder nodes that each hold the *full* content.
/// Incoming block requests are spread across the seeders round-robin, and each
/// seeder serves the block from its own copy while recording how many it served.
struct MultiSeederResolver {
    /// One block map per seeder node — all identical (every node has everything).
    seeders: Vec<HashMap<String, Vec<u8>>>,
    /// Number of blocks each seeder has served so far.
    served: StdMutex<Vec<usize>>,
    /// Round-robin cursor over the seeders.
    next: StdMutex<usize>,
}

impl MultiSeederResolver {
    fn new(seeder_count: usize, blocks: HashMap<String, Vec<u8>>) -> Self {
        Self {
            seeders: vec![blocks; seeder_count],
            served: StdMutex::new(vec![0; seeder_count]),
            next: StdMutex::new(0),
        }
    }
}

#[async_trait]
impl PeerResolver for MultiSeederResolver {
    async fn resolve_block(&self, cid: &str) -> Option<Vec<u8>> {
        let idx = {
            let mut next = self.next.lock().unwrap();
            let idx = *next % self.seeders.len();
            *next += 1;
            idx
        };
        // Serve from the chosen seeder's own copy and record the hit.
        let data = self.seeders[idx].get(cid).cloned();
        if data.is_some() {
            self.served.lock().unwrap()[idx] += 1;
        }
        data
    }
}

#[test]
fn test_chunks_are_served_distributed_across_seeders() {
    block_on(async {
        // Produce a multi-chunk file and capture all of its blocks.
        let origin = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            64 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        );
        let chunk_count = 9usize;
        // Distinct content per chunk so each maps to a unique CID (no dedup).
        let mut data = Vec::with_capacity(CHUNK_SIZE * chunk_count);
        for i in 0..chunk_count {
            data.extend(std::iter::repeat_n(i as u8, CHUNK_SIZE));
        }
        let root = origin.add("shared.bin", &data).await.unwrap();
        let content: HashMap<String, Vec<u8>> = origin.store.blocks.lock().unwrap().clone();

        // Three seeder nodes each hold the complete content.
        let seeder_count = 3;
        let resolver = MultiSeederResolver::new(seeder_count, content);
        let downloader = StorageEngine::new(
            MemBlockStore::new(),
            resolver,
            64 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        );

        // The downloading node retrieves the file purely from the seeders.
        let retrieved = downloader.get(&root).await.unwrap();
        assert_eq!(
            data, retrieved,
            "reassembled content must match the original"
        );

        // Each seeder must have contributed its share — no single node served
        // everything (which would be the centralized client/server case).
        let served = downloader.resolver.served.lock().unwrap().clone();
        let total: usize = served.iter().sum();
        assert_eq!(
            total,
            chunk_count + 1,
            "every block (chunks + manifest) is served exactly once: {served:?}"
        );
        assert!(
            served.iter().all(|&n| n > 0),
            "load was not distributed; some seeder served nothing: {served:?}"
        );
    });
}

#[test]
fn test_get_block_touches_lru_entry() {
    block_on(async {
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            15,
            None,
            SpatialPolicy::default(),
        );
        engine.store.store_block("a", b"0123456789").await.unwrap();
        engine.store.store_block("b", b"0123456789").await.unwrap();
        {
            let mut mgr = engine.manager.lock().unwrap();
            mgr.track_block("a", 10, None);
            mgr.track_block("b", 10, None);
        }

        // "a" is the oldest entry; reading it through get_block() must count
        // as a fresh access (bug: it used to bypass StorageManager entirely),
        // or it would be picked as the eviction victim below instead of "b".
        assert!(engine.get_block("a").await.unwrap().is_some());

        let victims = engine.manager.lock().unwrap().eviction_candidates();
        assert_eq!(
            victims,
            vec!["b".to_string()],
            "get_block() must touch the LRU entry it reads"
        );
    });
}

#[test]
fn test_add_enforces_capacity_before_finishing_large_file() {
    block_on(async {
        // Capacity for ~5 chunks. Seed one old chunk, then add a 12-chunk file:
        // capacity is exceeded partway through, long before the last chunk is
        // written. The old block must be evicted while later chunks of the new
        // file are still being stored, not deferred until the whole 12MB file
        // has landed (the pre-fix behavior, which only enforced once at the end).
        let cap = 5 * CHUNK_SIZE as u64;
        let engine = StorageEngine::new(
            LoggingBlockStore::new(),
            NullResolver,
            cap,
            None,
            SpatialPolicy::default(),
        );

        let old_data = vec![9u8; CHUNK_SIZE];
        let old_cid = compute_cid(&old_data, MULTICODEC_RAW);
        engine.add("old.bin", &old_data).await.unwrap();

        let chunk_count = 12usize;
        let mut new_data = Vec::with_capacity(CHUNK_SIZE * chunk_count);
        for i in 0..chunk_count {
            new_data.extend(std::iter::repeat_n(i as u8, CHUNK_SIZE));
        }
        let last_chunk = &new_data[(chunk_count - 1) * CHUNK_SIZE..];
        let last_chunk_cid = compute_cid(last_chunk, MULTICODEC_RAW);

        engine.add("new.bin", &new_data).await.unwrap();

        let log = engine.store.log.lock().unwrap();
        let evict_pos = log.iter().position(|e| e == &format!("delete:{old_cid}"));
        let last_store_pos = log
            .iter()
            .position(|e| e == &format!("store:{last_chunk_cid}"));

        assert!(evict_pos.is_some(), "old block was never evicted: {log:?}");
        assert!(
            last_store_pos.is_some(),
            "last chunk was never stored: {log:?}"
        );
        assert!(
            evict_pos.unwrap() < last_store_pos.unwrap(),
            "capacity enforcement was deferred until after the whole file was \
             written instead of running mid-loop: {log:?}"
        );
    });
}

/// A `SelfPositionSource` that always reports the same fixed list of
/// positions, standing in for whatever native/wasm derives from its session
/// registry (SPEC-15).
struct FixedPositionSource {
    positions: Vec<Vector3>,
}

impl FixedPositionSource {
    fn new(positions: Vec<Vector3>) -> Self {
        Self { positions }
    }
}

#[async_trait]
impl SelfPositionSource for FixedPositionSource {
    async fn self_positions(&self) -> Vec<Vector3> {
        self.positions.clone()
    }
}

#[test]
fn test_add_auto_tags_blocks_with_first_self_position() {
    block_on(async {
        let source: Arc<dyn SelfPositionSource> = Arc::new(FixedPositionSource::new(vec![
            Vector3::new(1.0, 2.0, 3.0),
            Vector3::new(9.0, 9.0, 9.0),
        ]));
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            Some(source),
            SpatialPolicy::default(),
        );

        let data = b"auto-tag me";
        let root_cid = engine.add("auto.bin", data).await.unwrap();
        let chunk_cid = compute_cid(data, MULTICODEC_RAW);

        let mgr = engine.manager.lock().unwrap();
        assert_eq!(
            mgr.peek_position(&chunk_cid),
            Some(Vector3::new(1.0, 2.0, 3.0)),
            "chunk must be auto-tagged with the *first* self position, not the second"
        );
        assert_eq!(
            mgr.peek_position(&root_cid),
            Some(Vector3::new(1.0, 2.0, 3.0)),
            "manifest must be auto-tagged too"
        );
    });
}

#[test]
fn test_add_at_explicit_position_overrides_auto_tag() {
    block_on(async {
        let source: Arc<dyn SelfPositionSource> =
            Arc::new(FixedPositionSource::new(vec![Vector3::new(1.0, 2.0, 3.0)]));
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            Some(source),
            SpatialPolicy::default(),
        );

        let data = b"explicit position";
        let explicit = Vector3::new(42.0, 0.0, 0.0);
        let root_cid = engine
            .add_at("explicit.bin", data, Some(explicit))
            .await
            .unwrap();

        let mgr = engine.manager.lock().unwrap();
        assert_eq!(mgr.peek_position(&root_cid), Some(explicit));
    });
}

#[test]
fn test_resolve_or_fetch_auto_tags_with_self_position() {
    block_on(async {
        let seeder = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            64 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        );
        let data = b"peer content";
        let root = seeder.add("peer.bin", data).await.unwrap();
        let seeded: HashMap<String, Vec<u8>> = seeder.store.blocks.lock().unwrap().clone();

        let resolver = CountingResolver::new(seeded);
        let source: Arc<dyn SelfPositionSource> =
            Arc::new(FixedPositionSource::new(vec![Vector3::new(5.0, 5.0, 5.0)]));
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            resolver,
            64 * 1024 * 1024,
            Some(source),
            SpatialPolicy::default(),
        );

        let retrieved = engine.get(&root).await.unwrap();
        assert_eq!(retrieved, data.to_vec());

        let chunk_cid = compute_cid(data, MULTICODEC_RAW);
        let mgr = engine.manager.lock().unwrap();
        assert_eq!(
            mgr.peek_position(&chunk_cid),
            Some(Vector3::new(5.0, 5.0, 5.0)),
            "block fetched from a peer must be auto-tagged with the self position"
        );
        assert_eq!(mgr.peek_position(&root), Some(Vector3::new(5.0, 5.0, 5.0)));
    });
}

#[test]
fn test_enforce_capacity_limit_prefers_spatial_eviction_when_source_present() {
    block_on(async {
        // Comfortably fits "near" + "far" (chunk + tiny manifest each); the
        // third block pushes usage past the cap and forces an eviction.
        let cap = 3 * CHUNK_SIZE as u64;
        let source: Arc<dyn SelfPositionSource> =
            Arc::new(FixedPositionSource::new(vec![Vector3::new(0.0, 0.0, 0.0)]));
        let engine = StorageEngine::new(
            LoggingBlockStore::new(),
            NullResolver,
            cap,
            Some(source),
            SpatialPolicy::default(), // retention_radius = 100.0
        );

        // "near" is added first (older, so plain LRU would pick it first) but
        // tagged right next to self (distance coefficient ~1.0). "far" is
        // younger but tagged far beyond the retention radius (huge distance
        // coefficient), so spatial eviction must pick "far" despite it being
        // the newer block.
        let near_data = vec![1u8; CHUNK_SIZE];
        let near_chunk_cid = compute_cid(&near_data, MULTICODEC_RAW);
        engine
            .add_at("near.bin", &near_data, Some(Vector3::new(1.0, 0.0, 0.0)))
            .await
            .unwrap();

        let far_data = vec![2u8; CHUNK_SIZE];
        let far_chunk_cid = compute_cid(&far_data, MULTICODEC_RAW);
        engine
            .add_at("far.bin", &far_data, Some(Vector3::new(10_000.0, 0.0, 0.0)))
            .await
            .unwrap();

        let trigger_data = vec![3u8; CHUNK_SIZE];
        engine.add("trigger.bin", &trigger_data).await.unwrap();

        let log = engine.store.log.lock().unwrap();
        assert!(
            log.iter().any(|e| e == &format!("delete:{far_chunk_cid}")),
            "the block tagged far beyond the retention radius must be evicted: {log:?}"
        );
        assert!(
            !log.iter().any(|e| e == &format!("delete:{near_chunk_cid}")),
            "the block tagged near self must be protected from eviction: {log:?}"
        );
    });
}

#[test]
fn test_run_decay_sweep_deletes_and_untracks_far_blocks() {
    block_on(async {
        let source: Arc<dyn SelfPositionSource> =
            Arc::new(FixedPositionSource::new(vec![Vector3::new(0.0, 0.0, 0.0)]));
        let spatial = SpatialPolicy {
            retention_radius: 10.0,
            decay_max_probability: 1.0,
        };
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            Some(source),
            spatial,
        );

        let data = b"far away block";
        let root_cid = engine
            .add_at("far.bin", data, Some(Vector3::new(1000.0, 0.0, 0.0)))
            .await
            .unwrap();

        // Both the chunk and the manifest are tagged at (1000,0,0), well
        // beyond 4*retention_radius (40.0), so with max_probability = 1.0
        // both must decay.
        let removed = engine.run_decay_sweep().await;
        assert_eq!(removed, 2, "chunk + manifest must both decay");
        assert_eq!(engine.manager.lock().unwrap().block_count(), 0);
        assert!(engine.store.load_block(&root_cid).await.unwrap().is_none());
    });
}

#[test]
fn test_run_decay_sweep_returns_zero_without_position_source() {
    block_on(async {
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        );
        engine
            .add_at("x.bin", b"data", Some(Vector3::new(999.0, 0.0, 0.0)))
            .await
            .unwrap();
        assert_eq!(engine.run_decay_sweep().await, 0);
    });
}

#[test]
fn test_run_decay_sweep_returns_zero_when_source_reports_no_positions() {
    block_on(async {
        let source: Arc<dyn SelfPositionSource> = Arc::new(FixedPositionSource::new(vec![]));
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            Some(source),
            SpatialPolicy::default(),
        );
        engine
            .add_at("x.bin", b"data", Some(Vector3::new(999.0, 0.0, 0.0)))
            .await
            .unwrap();
        assert_eq!(engine.run_decay_sweep().await, 0);
    });
}

// ---- SPEC-18: storage pinning ----------------------------------------

#[test]
fn test_pin_protects_from_lru_eviction() {
    block_on(async {
        let meta: Arc<dyn MetaStore> = Arc::new(MemMetaStore::new());
        let cap = 2 * CHUNK_SIZE as u64;
        let engine = StorageEngine::new(
            LoggingBlockStore::new(),
            NullResolver,
            cap,
            None,
            SpatialPolicy::default(),
        )
        .with_meta_store(meta);

        let pinned_data = vec![1u8; CHUNK_SIZE];
        let pinned_chunk_cid = compute_cid(&pinned_data, MULTICODEC_RAW);
        let pinned_root = engine.add("pinned.bin", &pinned_data).await.unwrap();
        engine.pin(&pinned_root).await.unwrap();

        let old_data = vec![2u8; CHUNK_SIZE];
        let old_chunk_cid = compute_cid(&old_data, MULTICODEC_RAW);
        engine.add("old.bin", &old_data).await.unwrap();

        // Pushes usage over `cap`; the LRU pass must skip the pinned root
        // entirely and evict the older *unpinned* block instead.
        let trigger_data = vec![3u8; CHUNK_SIZE];
        engine.add("trigger.bin", &trigger_data).await.unwrap();

        let log = engine.store.log.lock().unwrap();
        assert!(
            !log.iter()
                .any(|e| e == &format!("delete:{pinned_chunk_cid}")),
            "pinned chunk must never be evicted: {log:?}"
        );
        assert!(
            !log.iter().any(|e| e == &format!("delete:{pinned_root}")),
            "pinned manifest must never be evicted: {log:?}"
        );
        assert!(
            log.iter().any(|e| e == &format!("delete:{old_chunk_cid}")),
            "unpinned old chunk should have been evicted to make room: {log:?}"
        );
    });
}

#[test]
fn test_pin_protects_from_spatial_eviction() {
    block_on(async {
        let meta: Arc<dyn MetaStore> = Arc::new(MemMetaStore::new());
        let cap = 2 * CHUNK_SIZE as u64;
        let source: Arc<dyn SelfPositionSource> =
            Arc::new(FixedPositionSource::new(vec![Vector3::new(0.0, 0.0, 0.0)]));
        let engine = StorageEngine::new(
            LoggingBlockStore::new(),
            NullResolver,
            cap,
            Some(source),
            SpatialPolicy::default(), // retention_radius = 100.0
        )
        .with_meta_store(meta);

        // Both tagged equally far beyond the retention radius, so spatial
        // scoring alone can't tell them apart; "pinned_far" is also older
        // (added first), which would normally make it the *first* pick.
        let pinned_data = vec![1u8; CHUNK_SIZE];
        let pinned_chunk_cid = compute_cid(&pinned_data, MULTICODEC_RAW);
        let pinned_root = engine
            .add_at(
                "pinned_far.bin",
                &pinned_data,
                Some(Vector3::new(10_000.0, 0.0, 0.0)),
            )
            .await
            .unwrap();
        engine.pin(&pinned_root).await.unwrap();

        let unpinned_data = vec![2u8; CHUNK_SIZE];
        let unpinned_chunk_cid = compute_cid(&unpinned_data, MULTICODEC_RAW);
        engine
            .add_at(
                "unpinned_far.bin",
                &unpinned_data,
                Some(Vector3::new(10_000.0, 0.0, 0.0)),
            )
            .await
            .unwrap();

        let trigger_data = vec![3u8; CHUNK_SIZE];
        engine.add("trigger.bin", &trigger_data).await.unwrap();

        let log = engine.store.log.lock().unwrap();
        assert!(
            !log.iter()
                .any(|e| e == &format!("delete:{pinned_chunk_cid}")),
            "pinned chunk must be protected from spatial eviction even though \
             it scores at least as evictable as the unpinned one: {log:?}"
        );
        assert!(
            log.iter()
                .any(|e| e == &format!("delete:{unpinned_chunk_cid}")),
            "unpinned chunk at the same far position must be evicted instead: {log:?}"
        );
    });
}

#[test]
fn test_pin_protects_from_decay_sweep() {
    block_on(async {
        let meta: Arc<dyn MetaStore> = Arc::new(MemMetaStore::new());
        let source: Arc<dyn SelfPositionSource> =
            Arc::new(FixedPositionSource::new(vec![Vector3::new(0.0, 0.0, 0.0)]));
        let spatial = SpatialPolicy {
            retention_radius: 10.0,
            decay_max_probability: 1.0,
        };
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            Some(source),
            spatial,
        )
        .with_meta_store(meta);

        let data = b"pin me before decay";
        let root_cid = engine
            .add_at("far.bin", data, Some(Vector3::new(1000.0, 0.0, 0.0)))
            .await
            .unwrap();
        engine.pin(&root_cid).await.unwrap();

        // With max_probability = 1.0 and d >> 4*retention_radius, an
        // unpinned block at this position would certainly decay (see
        // test_run_decay_sweep_deletes_and_untracks_far_blocks). Pinning
        // must exempt it entirely.
        let removed = engine.run_decay_sweep().await;
        assert_eq!(removed, 0, "pinned blocks must never be selected for decay");
        assert_eq!(
            engine.manager.lock().unwrap().block_count(),
            2,
            "pinned chunk + manifest remain tracked"
        );
        assert!(engine.store.load_block(&root_cid).await.unwrap().is_some());
    });
}

#[test]
fn test_shared_chunk_protected_until_last_pinning_root_unpinned() {
    block_on(async {
        let meta: Arc<dyn MetaStore> = Arc::new(MemMetaStore::new());
        let cap = 2 * CHUNK_SIZE as u64;
        let engine = StorageEngine::new(
            LoggingBlockStore::new(),
            NullResolver,
            cap,
            None,
            SpatialPolicy::default(),
        )
        .with_meta_store(meta);

        // Two different files (different names -> different manifest/root
        // CIDs) with identical content, so CAS dedup makes both manifests
        // reference the same chunk CID.
        let shared_data = vec![9u8; CHUNK_SIZE];
        let shared_chunk_cid = compute_cid(&shared_data, MULTICODEC_RAW);
        let root_a = engine.add("a.bin", &shared_data).await.unwrap();
        let root_b = engine.add("b.bin", &shared_data).await.unwrap();
        assert_ne!(
            root_a, root_b,
            "different names must yield different manifest CIDs"
        );

        engine.pin(&root_a).await.unwrap();
        engine.pin(&root_b).await.unwrap();

        // Unpinning "a" alone must not lose the shared chunk's protection --
        // "b" still references it.
        engine.unpin(&root_a).await.unwrap();
        engine
            .add("trigger1.bin", &vec![1u8; CHUNK_SIZE])
            .await
            .unwrap();
        {
            let log = engine.store.log.lock().unwrap();
            assert!(
                !log.iter()
                    .any(|e| e == &format!("delete:{shared_chunk_cid}")),
                "chunk still referenced by pinned root b must survive: {log:?}"
            );
        }

        // Unpinning "b" too removes the last pin referencing the chunk; it
        // becomes a normal eviction candidate again.
        engine.unpin(&root_b).await.unwrap();
        engine
            .add("trigger2.bin", &vec![2u8; CHUNK_SIZE])
            .await
            .unwrap();
        {
            let log = engine.store.log.lock().unwrap();
            assert!(
                log.iter()
                    .any(|e| e == &format!("delete:{shared_chunk_cid}")),
                "chunk with no pinning root left should become evictable again: {log:?}"
            );
        }
    });
}

#[test]
fn test_add_pinned_is_protected_immediately_after_returning() {
    block_on(async {
        let meta: Arc<dyn MetaStore> = Arc::new(MemMetaStore::new());
        let cap = 2 * CHUNK_SIZE as u64;
        let engine = StorageEngine::new(
            LoggingBlockStore::new(),
            NullResolver,
            cap,
            None,
            SpatialPolicy::default(),
        )
        .with_meta_store(meta);

        let pinned_data = vec![1u8; CHUNK_SIZE];
        let pinned_chunk_cid = compute_cid(&pinned_data, MULTICODEC_RAW);
        let root = engine.add_pinned("pinned.bin", &pinned_data).await.unwrap();
        assert!(engine.is_pinned(&root).await);

        // Two more chunk-sized files right after: capacity is exceeded and
        // something must be evicted, but it must never be the block
        // add_pinned just wrote -- proving protection was already in effect
        // the instant add_pinned returned, with no gap for a subsequent
        // enforce_capacity_limit() to have raced it.
        engine
            .add("other1.bin", &vec![2u8; CHUNK_SIZE])
            .await
            .unwrap();
        engine
            .add("other2.bin", &vec![3u8; CHUNK_SIZE])
            .await
            .unwrap();

        let log = engine.store.log.lock().unwrap();
        assert!(
            !log.iter()
                .any(|e| e == &format!("delete:{pinned_chunk_cid}")),
            "add_pinned's own chunk must never be evicted: {log:?}"
        );
        assert!(
            !log.iter().any(|e| e == &format!("delete:{root}")),
            "add_pinned's own manifest must never be evicted: {log:?}"
        );
    });
}

#[test]
fn test_pin_registry_persists_and_reloads_across_engine_instances() {
    block_on(async {
        let meta: Arc<dyn MetaStore> = Arc::new(MemMetaStore::new());

        let root = {
            let engine = StorageEngine::new(
                MemBlockStore::new(),
                NullResolver,
                10 * 1024 * 1024,
                None,
                SpatialPolicy::default(),
            )
            .with_meta_store(meta.clone());
            let root = engine.add("persist.bin", b"keep me").await.unwrap();
            engine.pin(&root).await.unwrap();
            root
        };

        // Fresh engine instance, same MetaStore: pin state must survive
        // even though the (in-memory-only) LRU metadata does not.
        let engine2 = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        )
        .with_meta_store(meta);

        assert!(
            engine2.is_pinned(&root).await,
            "pin state must survive across engine instances sharing the same MetaStore"
        );
    });
}

/// Like `MemMetaStore`, but `get` reads its answer immediately and then
/// yields a few times before returning it -- a stale read, as when a real
/// backend has already fetched the bytes and the task is merely waiting to
/// be polled again. This keeps the load window inside
/// `ensure_pin_registry_loaded` open across sibling pin calls while pinning
/// (pun intended) the loaded snapshot to the pre-race state.
struct SlowGetMetaStore {
    inner: Arc<MemMetaStore>,
}

#[async_trait]
impl MetaStore for SlowGetMetaStore {
    async fn set(&self, key: &str, data: &[u8]) -> Result<()> {
        self.inner.set(key, data).await
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let snapshot = self.inner.get(key).await;
        for _ in 0..4 {
            YieldOnce(false).await;
        }
        snapshot
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(key).await
    }
}

#[test]
fn test_concurrent_pins_during_initial_registry_load_lose_nothing() {
    block_on(async {
        let mem = Arc::new(MemMetaStore::new());

        // Seed a persisted registry containing one pin from a previous
        // "session".
        let seeded = {
            let engine = StorageEngine::new(
                MemBlockStore::new(),
                NullResolver,
                10 * 1024 * 1024,
                None,
                SpatialPolicy::default(),
            )
            .with_meta_store(mem.clone());
            let root = engine.add("seed.bin", b"seed").await.unwrap();
            engine.pin(&root).await.unwrap();
            root
        };

        // Fresh engine whose first two pin calls overlap the initial
        // registry load (get yields before answering). Without the load
        // being serialized, the second caller re-reads the pre-load
        // snapshot, overwrites the first caller's freshly added pin, and
        // persists that rollback -- silently un-pinning it.
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        )
        .with_meta_store(Arc::new(SlowGetMetaStore { inner: mem.clone() }));

        let (a, b) = futures::join!(
            engine.add_pinned("a.bin", b"aaa"),
            engine.add_pinned("b.bin", b"bbb"),
        );
        let (a, b) = (a.unwrap(), b.unwrap());

        // A fresh engine reading the persisted registry must see all three.
        let engine2 = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        )
        .with_meta_store(mem);
        for (label, cid) in [("seeded", &seeded), ("first", &a), ("second", &b)] {
            assert!(
                engine2.is_pinned(cid).await,
                "{label} pin was lost by a concurrent initial registry load"
            );
        }
    });
}

#[test]
fn test_corrupted_pin_registry_errors_without_clearing_existing_pins() {
    block_on(async {
        let meta: Arc<dyn MetaStore> = Arc::new(MemMetaStore::new());
        meta.set(PIN_REGISTRY_META_KEY, b"not valid json")
            .await
            .unwrap();

        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        )
        .with_meta_store(meta);

        // is_pinned can't propagate the load error (its contract is a plain
        // bool), so an unreadable registry reports "not pinned" rather than
        // being trusted as if it were empty-and-valid.
        assert!(!engine.is_pinned("whatever").await);

        // pin/unpin *can* propagate the error, and must -- proceeding as if
        // the corrupt registry were simply empty would silently unpin
        // everything it used to protect.
        let root = engine.add("x.bin", b"data").await.unwrap();
        assert!(engine.pin(&root).await.is_err());
    });
}

#[test]
fn test_pin_without_meta_store_errors_and_eviction_is_unaffected() {
    block_on(async {
        let cap = 2 * CHUNK_SIZE as u64;
        let engine = StorageEngine::new(
            LoggingBlockStore::new(),
            NullResolver,
            cap,
            None,
            SpatialPolicy::default(),
        ); // no .with_meta_store()

        let old_data = vec![1u8; CHUNK_SIZE];
        let old_chunk_cid = compute_cid(&old_data, MULTICODEC_RAW);
        let root = engine.add("old.bin", &old_data).await.unwrap();

        assert!(
            engine.pin(&root).await.is_err(),
            "pin must fail without a MetaStore"
        );
        assert!(
            !engine.is_pinned(&root).await,
            "is_pinned is false without a MetaStore"
        );
        assert!(engine.add_pinned("y.bin", b"data").await.is_err());

        // Eviction must behave exactly as it did before SPEC-18: nothing is
        // exempt when there's no meta store to persist a pin against.
        engine
            .add("trigger1.bin", &vec![2u8; CHUNK_SIZE])
            .await
            .unwrap();
        engine
            .add("trigger2.bin", &vec![3u8; CHUNK_SIZE])
            .await
            .unwrap();

        let log = engine.store.log.lock().unwrap();
        assert!(
            log.iter().any(|e| e == &format!("delete:{old_chunk_cid}")),
            "without pinning, the old block must be evicted like before SPEC-18: {log:?}"
        );
    });
}

#[test]
fn test_pin_and_unpin_are_idempotent() {
    block_on(async {
        let meta: Arc<dyn MetaStore> = Arc::new(MemMetaStore::new());
        let engine = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            None,
            SpatialPolicy::default(),
        )
        .with_meta_store(meta);

        let root = engine.add("x.bin", b"data").await.unwrap();

        engine.pin(&root).await.unwrap();
        engine.pin(&root).await.unwrap(); // double pin: still Ok
        assert!(engine.is_pinned(&root).await);

        engine.unpin(&root).await.unwrap();
        engine.unpin(&root).await.unwrap(); // double unpin: still Ok
        assert!(!engine.is_pinned(&root).await);

        // Unpinning something that was never pinned: also Ok.
        let never_pinned = engine.add("y.bin", b"other").await.unwrap();
        assert!(engine.unpin(&never_pinned).await.is_ok());
    });
}

/// Regression test: `enforce_capacity_limit` used to read
/// `manager.eviction_candidates()` directly without ever calling
/// `ensure_pin_registry_loaded()` first (unlike `pin`/`unpin`/`is_pinned`,
/// which all do). On a freshly constructed `StorageEngine` -- e.g. right
/// after a restart, before the app happens to call any pin API -- the
/// in-memory `pinned` set starts empty even though the persisted
/// `PinRegistry` correctly protects a block, so capacity eviction could
/// delete it anyway.
#[test]
fn test_enforce_capacity_limit_loads_pin_registry_on_fresh_engine() {
    block_on(async {
        let meta: Arc<dyn MetaStore> = Arc::new(MemMetaStore::new());

        // "Session 1": add and pin a file; the pin registry entry is
        // persisted to `meta`.
        let pinned_data = vec![1u8; CHUNK_SIZE];
        let pinned_chunk_cid = compute_cid(&pinned_data, MULTICODEC_RAW);
        let pinned_root = {
            let engine = StorageEngine::new(
                MemBlockStore::new(),
                NullResolver,
                10 * 1024 * 1024,
                None,
                SpatialPolicy::default(),
            )
            .with_meta_store(meta.clone());
            let root = engine.add("pinned.bin", &pinned_data).await.unwrap();
            engine.pin(&root).await.unwrap();
            root
        };

        // "Session 2": a freshly constructed engine sharing the same
        // MetaStore (simulating a restart), whose in-memory LRU tracking has
        // rediscovered the pinned block on disk (e.g. scanning existing
        // storage at startup) but which has NOT called pin()/unpin()/
        // is_pinned() yet -- so `pin_registry_loaded` is still false and
        // `manager.pinned` is still empty even though the persisted
        // PinRegistry already protects this block.
        let cap = 1; // forces eviction the moment anything is tracked
        let engine2 = StorageEngine::new(
            LoggingBlockStore::new(),
            NullResolver,
            cap,
            None,
            SpatialPolicy::default(),
        )
        .with_meta_store(meta);

        engine2
            .store
            .store_block(&pinned_root, b"manifest-stand-in")
            .await
            .unwrap();
        engine2
            .store
            .store_block(&pinned_chunk_cid, &pinned_data)
            .await
            .unwrap();
        {
            let mut mgr = engine2.manager.lock().unwrap();
            mgr.track_block(&pinned_root, 18, None);
            mgr.track_block(&pinned_chunk_cid, CHUNK_SIZE as u64, None);
        }

        // Without loading the persisted registry first, both blocks look
        // like ordinary unpinned LRU victims and would be evicted here.
        engine2.enforce_capacity_limit().await.unwrap();

        let log = engine2.store.log.lock().unwrap();
        assert!(
            !log.iter()
                .any(|e| e == &format!("delete:{pinned_chunk_cid}")),
            "pinned chunk must survive capacity eviction on a fresh engine \
             instance that hasn't called any pin API yet: {log:?}"
        );
        assert!(
            !log.iter().any(|e| e == &format!("delete:{pinned_root}")),
            "pinned manifest must survive capacity eviction on a fresh engine \
             instance that hasn't called any pin API yet: {log:?}"
        );
    });
}

/// Same regression as above, but for `run_decay_sweep` (the other eviction
/// path that reads `manager.decay_candidates()` directly).
#[test]
fn test_run_decay_sweep_loads_pin_registry_on_fresh_engine() {
    block_on(async {
        let meta: Arc<dyn MetaStore> = Arc::new(MemMetaStore::new());
        let spatial = SpatialPolicy {
            retention_radius: 10.0,
            decay_max_probability: 1.0,
        };
        let far_position = Vector3::new(1000.0, 0.0, 0.0);

        // "Session 1": add and pin a file tagged far from the origin, so an
        // unpinned block at this position would certainly decay.
        let data = b"pin me before restart";
        let pinned_root = {
            let source: Arc<dyn SelfPositionSource> =
                Arc::new(FixedPositionSource::new(vec![Vector3::new(0.0, 0.0, 0.0)]));
            let engine = StorageEngine::new(
                MemBlockStore::new(),
                NullResolver,
                10 * 1024 * 1024,
                Some(source),
                spatial,
            )
            .with_meta_store(meta.clone());
            let root = engine
                .add_at("far.bin", data, Some(far_position))
                .await
                .unwrap();
            engine.pin(&root).await.unwrap();
            root
        };
        let chunk_cid = compute_cid(data, MULTICODEC_RAW);

        // "Session 2": fresh engine, same MetaStore, positions source active,
        // and the block re-tracked (as if rediscovered on disk at startup)
        // but no pin API called yet on this instance.
        let source: Arc<dyn SelfPositionSource> =
            Arc::new(FixedPositionSource::new(vec![Vector3::new(0.0, 0.0, 0.0)]));
        let engine2 = StorageEngine::new(
            MemBlockStore::new(),
            NullResolver,
            10 * 1024 * 1024,
            Some(source),
            spatial,
        )
        .with_meta_store(meta);

        engine2
            .store
            .store_block(&pinned_root, b"manifest-stand-in")
            .await
            .unwrap();
        engine2.store.store_block(&chunk_cid, data).await.unwrap();
        {
            let mut mgr = engine2.manager.lock().unwrap();
            mgr.track_block(&pinned_root, 18, Some(far_position));
            mgr.track_block(&chunk_cid, data.len() as u64, Some(far_position));
        }

        // Without loading the persisted registry first, both blocks look
        // decay-eligible (tagged far away, max_probability = 1.0) and would
        // be swept here.
        let removed = engine2.run_decay_sweep().await;
        assert_eq!(
            removed, 0,
            "pinned blocks must never be swept, even on a fresh engine \
             instance that hasn't called any pin API yet"
        );
        assert!(engine2
            .store
            .load_block(&pinned_root)
            .await
            .unwrap()
            .is_some());
        assert!(engine2
            .store
            .load_block(&chunk_cid)
            .await
            .unwrap()
            .is_some());
    });
}
