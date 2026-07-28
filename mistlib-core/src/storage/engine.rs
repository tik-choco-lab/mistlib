use super::backend::{BlockStore, MetaStore, PeerResolver, SelfPositionSource};
use super::cid::{compute_cid, verify_cid, MULTICODEC_DAG_CBOR, MULTICODEC_RAW};
use super::pin::{PinRegistry, PIN_REGISTRY_META_KEY};
use super::types::{FileManifest, StorageManager, CHUNK_SIZE};
use crate::config::StorageConfig;
use crate::error::{MistError, Result};
use crate::types::Vector3;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Spatial-eviction knobs threaded from `StorageConfig` down to
/// `StorageManager`. The decay sweep *interval* is deliberately not part of
/// this policy: driving the periodic sweep is native/wasm's job (core only
/// exposes `StorageEngine::run_decay_sweep` to be called on a timer).
#[derive(Debug, Clone, Copy)]
pub struct SpatialPolicy {
    pub retention_radius: f32,
    pub decay_max_probability: f32,
}

impl From<&StorageConfig> for SpatialPolicy {
    fn from(cfg: &StorageConfig) -> Self {
        // Clamp misconfigured values: a negative radius would flip the sign
        // of the decay formula and silently disable decay (the opposite of
        // the documented "0 = no protection"), and a probability outside
        // [0, 1] is meaningless.
        Self {
            retention_radius: cfg.spatial_retention_radius.max(0.0),
            decay_max_probability: cfg.spatial_decay_max_probability.clamp(0.0, 1.0),
        }
    }
}

impl Default for SpatialPolicy {
    fn default() -> Self {
        Self::from(&StorageConfig::default())
    }
}

pub struct StorageEngine<B: BlockStore, P: PeerResolver> {
    store: B,
    resolver: P,
    manager: Mutex<StorageManager>,
    position_source: Option<Arc<dyn SelfPositionSource>>,
    spatial: SpatialPolicy,
    sweep_counter: AtomicU64,
    meta: Option<Arc<dyn MetaStore>>,
    // Pin registry state (SPEC-18). Loaded lazily from `meta` on first
    // pin-related call (see `ensure_pin_registry_loaded`) rather than in
    // `new`/`with_meta_store`, since loading is async and those aren't.
    pin_registry: Mutex<PinRegistry>,
    pin_registry_loaded: AtomicBool,
    // Serializes the initial registry load and every persist (snapshot +
    // write) so concurrent pin/unpin calls can't clobber each other's
    // updates -- see `ensure_pin_registry_loaded`/`persist_pin_registry`.
    pin_registry_io: tokio::sync::Mutex<()>,
}

impl<B: BlockStore, P: PeerResolver> StorageEngine<B, P> {
    pub fn new(
        store: B,
        resolver: P,
        max_capacity_bytes: u64,
        position_source: Option<Arc<dyn SelfPositionSource>>,
        spatial: SpatialPolicy,
    ) -> Self {
        Self {
            store,
            resolver,
            manager: Mutex::new(StorageManager::new(max_capacity_bytes)),
            position_source,
            spatial,
            sweep_counter: AtomicU64::new(0),
            meta: None,
            pin_registry: Mutex::new(PinRegistry::new()),
            pin_registry_loaded: AtomicBool::new(false),
            pin_registry_io: tokio::sync::Mutex::new(()),
        }
    }

    /// Attaches the mutable metadata backend (SPEC-17). Builder-style rather
    /// than a `new` parameter so the widely-used constructor signature stays
    /// stable; features that must persist across restarts (pinning, SPEC-18)
    /// require it and error without it.
    pub fn with_meta_store(mut self, meta: Arc<dyn MetaStore>) -> Self {
        self.meta = Some(meta);
        self
    }

    pub fn meta_store(&self) -> Option<&Arc<dyn MetaStore>> {
        self.meta.as_ref()
    }

    pub async fn get_block(&self, cid: &str) -> Result<Option<Vec<u8>>> {
        let block = self.store.load_block(cid).await?;
        if block.is_some() {
            self.manager.lock().unwrap().touch(cid);
        }
        Ok(block)
    }

    /// Resolves the position to auto-tag a freshly tracked block with when no
    /// explicit position was given: the first (join-order) session's current
    /// position, or `None` if there's no source or no session has reported
    /// one yet.
    async fn resolve_auto_position(&self) -> Option<Vector3> {
        match &self.position_source {
            Some(source) => source.self_positions().await.into_iter().next(),
            None => None,
        }
    }

    pub async fn add(&self, name: &str, data: &[u8]) -> Result<String> {
        self.add_at(name, data, None).await
    }

    /// Like `add`, but with an explicit spatial tag for every chunk and the
    /// manifest. `position: None` falls back to auto-tagging via the
    /// configured `SelfPositionSource` (see `resolve_auto_position`).
    pub async fn add_at(
        &self,
        name: &str,
        data: &[u8],
        position: Option<Vector3>,
    ) -> Result<String> {
        let (root_cid, _chunk_cids) = self.add_at_inner(name, data, position).await?;
        Ok(root_cid)
    }

    /// Shared write path for `add_at` and `add_pinned` (SPEC-18). Returns
    /// both the root CID and the full chunk CID list so `add_pinned` can
    /// register the pin registry entry without re-deriving or re-fetching
    /// anything: the chunk list is already in hand the moment this returns.
    async fn add_at_inner(
        &self,
        name: &str,
        data: &[u8],
        position: Option<Vector3>,
    ) -> Result<(String, Vec<String>)> {
        debug!("StorageEngine::add: name={}, size={}", name, data.len());
        let position = match position {
            Some(p) => Some(p),
            None => self.resolve_auto_position().await,
        };

        use futures_util::stream::{FuturesUnordered, StreamExt};

        // Mirrors get()'s bounded-concurrency pattern: at most 4 chunk writes
        // in flight at once. enforce_capacity_limit() runs after each drain so
        // a large add() can't blow past max_capacity_bytes until the whole
        // file has been written.
        //
        // A block only becomes tracked (= an eviction candidate) once its
        // write has actually completed: FuturesUnordered resolves in
        // completion order, so tracking at submit time could let the LRU
        // evict a cid whose write is still in flight — the delete would hit
        // nothing and the late write would then resurrect the block untracked.
        // The cost is that usage accounting lags the in-flight writes by up
        // to `concurrency` chunks, which is bounded and acceptable.
        let concurrency = 4;
        let mut chunk_cids = Vec::new();
        let mut writes = FuturesUnordered::new();

        for chunk in data.chunks(CHUNK_SIZE) {
            let cid = compute_cid(chunk, MULTICODEC_RAW);
            chunk_cids.push(cid.clone());

            writes.push(async move {
                let len = chunk.len() as u64;
                self.store
                    .store_block(&cid, chunk)
                    .await
                    .map(|()| (cid, len))
            });

            if writes.len() >= concurrency {
                if let Some(res) = writes.next().await {
                    let (done_cid, len) = res?;
                    self.manager
                        .lock()
                        .unwrap()
                        .track_block(&done_cid, len, position);
                }
                self.enforce_capacity_limit().await?;
            }
        }

        while let Some(res) = writes.next().await {
            let (done_cid, len) = res?;
            self.manager
                .lock()
                .unwrap()
                .track_block(&done_cid, len, position);
            self.enforce_capacity_limit().await?;
        }

        let manifest = FileManifest {
            name: name.to_string(),
            size: data.len() as u64,
            chunks: chunk_cids.clone(),
        };
        let manifest_bytes =
            serde_cbor::to_vec(&manifest).map_err(|e| MistError::Serialization(e.to_string()))?;
        let root_cid = compute_cid(&manifest_bytes, MULTICODEC_DAG_CBOR);

        self.store.store_block(&root_cid, &manifest_bytes).await?;
        {
            let mut mgr = self.manager.lock().unwrap();
            mgr.track_block(&root_cid, manifest_bytes.len() as u64, position);
        }
        self.enforce_capacity_limit().await?;
        Ok((root_cid, chunk_cids))
    }

    pub async fn get(&self, root_cid: &str) -> Result<Vec<u8>> {
        info!("StorageEngine::get: cid={}", root_cid);
        let manifest_bytes = self.resolve_or_fetch(root_cid, MULTICODEC_DAG_CBOR).await?;
        let manifest: FileManifest = serde_cbor::from_slice(&manifest_bytes)
            .map_err(|e| MistError::Serialization(e.to_string()))?;

        info!(
            "StorageEngine: downloading '{}' ({} bytes, {} chunks)",
            manifest.name,
            manifest.size,
            manifest.chunks.len()
        );

        use futures_util::stream::{FuturesUnordered, StreamExt};
        use std::collections::BTreeMap;

        let concurrency = 4;
        let mut pending: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
        let mut futures = FuturesUnordered::new();
        let mut next_index = 0usize;
        let mut result = Vec::with_capacity(manifest.size as usize);

        let drain =
            |result: &mut Vec<u8>, pending: &mut BTreeMap<usize, Vec<u8>>, next: &mut usize| {
                while let Some(chunk) = pending.remove(next) {
                    result.extend_from_slice(&chunk);
                    *next += 1;
                }
            };

        for (i, cid) in manifest.chunks.iter().enumerate() {
            let cid = cid.clone();
            futures.push(async move {
                let data = self.resolve_or_fetch(&cid, MULTICODEC_RAW).await?;
                Ok::<(usize, Vec<u8>), MistError>((i, data))
            });

            if futures.len() >= concurrency {
                if let Some(res) = futures.next().await {
                    let (idx, data) = res?;
                    if idx == next_index {
                        result.extend_from_slice(&data);
                        next_index += 1;
                        drain(&mut result, &mut pending, &mut next_index);
                    } else {
                        pending.insert(idx, data);
                    }
                }
            }
        }

        while let Some(res) = futures.next().await {
            let (idx, data) = res?;
            if idx == next_index {
                result.extend_from_slice(&data);
                next_index += 1;
                drain(&mut result, &mut pending, &mut next_index);
            } else {
                pending.insert(idx, data);
            }
        }

        info!("StorageEngine: successfully retrieved '{}'", manifest.name);
        Ok(result)
    }

    pub async fn enforce_capacity_limit(&self) -> Result<()> {
        // Fetched before locking `manager`: `self_positions()` is async and
        // `manager` is a plain `std::sync::Mutex`, which must never be held
        // across an `.await` point.
        let positions = self.resolve_positions_for_eviction().await;

        // The in-memory pinned set must reflect the persisted PinRegistry
        // before computing eviction candidates (SPEC-18): a freshly
        // constructed engine starts with an empty pinned set, which would
        // let capacity eviction delete blocks that are actually pinned.
        // Gated on actually being over capacity so a write that was never
        // going to evict anything doesn't fail just because the persisted
        // registry happens to be unreadable (mirrors `is_pinned`'s
        // never-fails contract for the no-op case; see
        // `ensure_pin_registry_loaded`).
        let over_capacity = {
            let mgr = self.manager.lock().unwrap();
            mgr.current_usage() > mgr.max_capacity()
        };
        if over_capacity {
            self.ensure_pin_registry_loaded().await?;
        }

        let victims = {
            let mgr = self.manager.lock().unwrap();
            if positions.is_empty() {
                mgr.eviction_candidates()
            } else {
                mgr.spatial_eviction_candidates(&positions, self.spatial.retention_radius)
            }
        };
        for cid in victims {
            info!("StorageEngine: Evicting {}", cid);
            self.store.delete_block(&cid).await?;
            let mut mgr = self.manager.lock().unwrap();
            mgr.untrack_block(&cid);
        }

        // Pinned blocks (SPEC-18) are excluded from `victims` above, so once
        // every unpinned block is gone, usage can still sit above capacity
        // if pinned data alone accounts for the excess. Pin wins over the
        // capacity limit by design (see SPEC-18 "モデル"); this is an
        // operator signal, not an error -- the write that got us here
        // already succeeded and nothing here should fail it.
        let mgr = self.manager.lock().unwrap();
        if mgr.current_usage() > mgr.max_capacity() {
            warn!(
                "StorageEngine: over capacity ({} / {} bytes) with no unpinned blocks left to \
                 evict; pinned data accounts for the remainder",
                mgr.current_usage(),
                mgr.max_capacity()
            );
        }
        Ok(())
    }

    /// Runs one decay sweep: probabilistically deletes spatially-tagged
    /// blocks far from every current self-position (see
    /// `StorageManager::decay_candidates`). Driving this on a timer is
    /// native/wasm's responsibility; core just does the one sweep and
    /// reports how many blocks it removed.
    pub async fn run_decay_sweep(&self) -> usize {
        let positions = self.resolve_positions_for_eviction().await;
        if positions.is_empty() {
            return 0;
        }

        // Same rationale as `enforce_capacity_limit`: without this, a
        // freshly constructed engine's empty in-memory pinned set would let
        // the decay sweep delete blocks the persisted PinRegistry protects.
        // Gated on there actually being positions to sweep against (same
        // early-return above) so a no-op sweep can't fail just because the
        // persisted registry happens to be unreadable. Can't propagate an
        // error either way -- this fn returns `usize` -- so a load failure
        // just skips this sweep rather than risking an unprotected decay
        // pass; the next sweep retries the load.
        if let Err(e) = self.ensure_pin_registry_loaded().await {
            warn!(
                "StorageEngine: decay sweep skipped; pin registry load failed: {}",
                e
            );
            return 0;
        }

        let sweep = self.sweep_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let victims = {
            let mgr = self.manager.lock().unwrap();
            mgr.decay_candidates(
                &positions,
                self.spatial.retention_radius,
                self.spatial.decay_max_probability,
                sweep,
            )
        };

        let mut evicted = 0;
        for cid in victims {
            match self.store.delete_block(&cid).await {
                Ok(()) => {
                    self.manager.lock().unwrap().untrack_block(&cid);
                    evicted += 1;
                }
                Err(e) => {
                    warn!("StorageEngine: decay sweep failed to delete {}: {}", cid, e);
                }
            }
        }
        evicted
    }

    /// Pins `root_cid` (SPEC-18): its manifest and every chunk it
    /// enumerates become permanently exempt from eviction/decay until
    /// `unpin`. Resolves the manifest locally first, falling back to peers
    /// (`resolve_or_fetch`) like `get` does -- an unresolvable root can't be
    /// pinned, since promising to protect something that doesn't exist
    /// would be a lie. Idempotent: pinning an already-pinned root succeeds
    /// without re-resolving anything.
    ///
    /// Requires a `MetaStore` (`with_meta_store`): pin state that can't
    /// survive a restart isn't a promise this method is willing to make.
    pub async fn pin(&self, root_cid: &str) -> Result<()> {
        let meta = self.meta.clone().ok_or_else(|| {
            MistError::Internal("pin requires a MetaStore (StorageEngine::with_meta_store)".into())
        })?;
        self.ensure_pin_registry_loaded().await?;

        if self.pin_registry.lock().unwrap().is_pinned(root_cid) {
            return Ok(());
        }

        let manifest_bytes = self.resolve_or_fetch(root_cid, MULTICODEC_DAG_CBOR).await?;
        let manifest: FileManifest = serde_cbor::from_slice(&manifest_bytes)
            .map_err(|e| MistError::Serialization(e.to_string()))?;

        let mut cids = manifest.chunks;
        cids.push(root_cid.to_string());

        // Reflect protection into `StorageManager` before persisting: if the
        // MetaStore write below fails, this process still won't evict what
        // it just promised to pin (fail toward over-protecting, not
        // under-protecting). The caller does see the error and can retry.
        let pinned_cids = {
            let mut registry = self.pin_registry.lock().unwrap();
            registry.pin(root_cid, cids);
            registry.pinned_cids()
        };
        self.manager.lock().unwrap().set_pinned(pinned_cids);
        self.persist_pin_registry(&meta).await
    }

    /// Removes `root_cid`'s pin (SPEC-18). A chunk shared with another still
    /// -pinned root stays protected -- see `PinRegistry::pinned_cids`.
    /// Idempotent: unpinning a root that isn't pinned succeeds as a no-op.
    ///
    /// Requires a `MetaStore`, like `pin`.
    pub async fn unpin(&self, root_cid: &str) -> Result<()> {
        let meta = self.meta.clone().ok_or_else(|| {
            MistError::Internal(
                "unpin requires a MetaStore (StorageEngine::with_meta_store)".into(),
            )
        })?;
        self.ensure_pin_registry_loaded().await?;

        // Persist before relaxing in-memory protection: if the MetaStore
        // write fails, this process keeps protecting the block until a
        // retry succeeds, rather than dropping protection it couldn't
        // durably record (the opposite ordering from `pin`, for the same
        // fail-safe reason -- removing protection is the risky direction
        // here).
        let bytes = {
            let mut registry = self.pin_registry.lock().unwrap();
            registry.unpin(root_cid);
            registry.to_json()?
        };
        meta.set(PIN_REGISTRY_META_KEY, &bytes).await?;

        let pinned_cids = self.pin_registry.lock().unwrap().pinned_cids();
        self.manager.lock().unwrap().set_pinned(pinned_cids);
        Ok(())
    }

    /// Whether `root_cid` is currently pinned (SPEC-18). Never fails: absent
    /// a `MetaStore`, or if the persisted registry turns out to be
    /// unreadable, this reports `false` rather than propagating an error a
    /// caller can't easily act on -- see `ensure_pin_registry_loaded`.
    pub async fn is_pinned(&self, root_cid: &str) -> bool {
        if self.ensure_pin_registry_loaded().await.is_err() {
            return false;
        }
        self.pin_registry.lock().unwrap().is_pinned(root_cid)
    }

    /// `add_at` + pin, with no gap between them for a concurrent eviction to
    /// exploit (SPEC-18): rather than calling the public `add`/`pin` in
    /// sequence (which would let another task's `enforce_capacity_limit`
    /// run in between and evict a just-written, not-yet-pinned chunk), this
    /// registers the pin registry entry using the chunk CID list
    /// `add_at_inner` already computed, synchronously, before this function
    /// returns -- so no caller ever observes a root CID that isn't already
    /// protected.
    ///
    /// Requires a `MetaStore`, like `pin`; checked up front so a doomed call
    /// fails before writing any data.
    pub async fn add_pinned(&self, name: &str, data: &[u8]) -> Result<String> {
        let meta = self.meta.clone().ok_or_else(|| {
            MistError::Internal(
                "add_pinned requires a MetaStore (StorageEngine::with_meta_store)".into(),
            )
        })?;
        self.ensure_pin_registry_loaded().await?;

        let (root_cid, mut cids) = self.add_at_inner(name, data, None).await?;
        cids.push(root_cid.clone());

        let pinned_cids = {
            let mut registry = self.pin_registry.lock().unwrap();
            registry.pin(&root_cid, cids);
            registry.pinned_cids()
        };
        self.manager.lock().unwrap().set_pinned(pinned_cids);
        self.persist_pin_registry(&meta).await?;
        Ok(root_cid)
    }

    /// Loads the pin registry from `meta` on first use and reflects its
    /// protection set into `StorageManager::set_pinned`. A no-op on every
    /// call after the first success, and also a no-op (not an error) if no
    /// `MetaStore` is attached -- pin state simply can't exist yet in that
    /// case. A load failure (corrupt registry -- see `PinRegistry::from_json`)
    /// is propagated *without* marking the load complete, so the in-memory
    /// pin set stays whatever it already was (empty, on a fresh engine)
    /// rather than being clobbered by an unreadable persisted value, and the
    /// next call retries the load.
    async fn ensure_pin_registry_loaded(&self) -> Result<()> {
        if self.pin_registry_loaded.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(meta) = &self.meta else {
            return Ok(());
        };
        // Serialize the load and re-check under the lock: a second caller
        // arriving mid-load would otherwise re-read the pre-load snapshot
        // and overwrite pins the first caller added after its load finished
        // (lost update, silently un-pinning persisted data).
        let _guard = self.pin_registry_io.lock().await;
        if self.pin_registry_loaded.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(bytes) = meta.get(PIN_REGISTRY_META_KEY).await? {
            let registry = PinRegistry::from_json(&bytes)?;
            let pinned_cids = registry.pinned_cids();
            *self.pin_registry.lock().unwrap() = registry;
            self.manager.lock().unwrap().set_pinned(pinned_cids);
        }
        self.pin_registry_loaded.store(true, Ordering::Release);
        Ok(())
    }

    async fn persist_pin_registry(&self, meta: &Arc<dyn MetaStore>) -> Result<()> {
        // Snapshot and write under the same lock as the initial load, so two
        // concurrent persists can't land in reversed order and roll the
        // stored registry back to a stale snapshot.
        let _guard = self.pin_registry_io.lock().await;
        let bytes = self.pin_registry.lock().unwrap().to_json()?;
        meta.set(PIN_REGISTRY_META_KEY, &bytes).await
    }

    async fn resolve_positions_for_eviction(&self) -> Vec<Vector3> {
        match &self.position_source {
            Some(source) => source.self_positions().await,
            None => Vec::new(),
        }
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
        let position = self.resolve_auto_position().await;
        {
            let mut mgr = self.manager.lock().unwrap();
            mgr.track_block(cid, data.len() as u64, position);
        }
        self.enforce_capacity_limit().await?;
        self.store.store_block(cid, &data).await?;
        Ok(data)
    }
}

#[cfg(test)]
mod tests;
