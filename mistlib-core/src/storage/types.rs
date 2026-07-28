use crate::types::Vector3;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::hash::{Hash, Hasher};

pub const CHUNK_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileManifest {
    pub name: String,
    pub size: u64,
    pub chunks: Vec<String>,
}

pub struct StorageManager {
    max_capacity_bytes: u64,
    current_usage: u64,
    blocks_meta: HashMap<String, BlockMeta>,
    lru_index: BTreeMap<u64, String>,
    clock: u64,
    // Pin protection set (SPEC-18): CIDs excluded from all three eviction
    // paths below. Populated by `StorageEngine` from its `PinRegistry`, not
    // mutated here directly -- this struct only enforces the filter.
    // Pinned blocks still count toward `current_usage` (they occupy real
    // space); they just never appear as victims.
    pinned: HashSet<String>,
}

struct BlockMeta {
    last_accessed: u64,
    size: u64,
    // Not (de)serialized: spatial tagging is an in-memory-only hint, same
    // lifetime as the rest of this LRU metadata (see SPEC-16 "制限").
    position: Option<Vector3>,
}

/// Entry used to rank blocks by `effective_age` (see
/// `StorageManager::spatial_eviction_candidates`) in a max-heap. `effective_age`
/// is always finite and never NaN (see `distance_ratio`), so `Ord` can be
/// derived from `f64::total_cmp` safely.
struct ScoredBlock {
    effective_age: f64,
    size: u64,
    cid: String,
}

impl PartialEq for ScoredBlock {
    fn eq(&self, other: &Self) -> bool {
        self.effective_age == other.effective_age
    }
}
impl Eq for ScoredBlock {}
impl PartialOrd for ScoredBlock {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ScoredBlock {
    fn cmp(&self, other: &Self) -> Ordering {
        self.effective_age.total_cmp(&other.effective_age)
    }
}

/// `d / r`, guarding the two ways this would otherwise produce `NaN`:
/// - `d == 0.0` (block co-located with self) is always ratio `0.0`, regardless
///   of `r` (including `r == 0.0`, which would otherwise be `0.0 / 0.0`).
/// - `d > 0.0` with `r == 0.0` legitimately yields `+inf` (no protection
///   radius at all), which sorts and scales fine without being `NaN`.
fn distance_ratio(d: f32, r: f32) -> f32 {
    if d <= f32::EPSILON {
        0.0
    } else {
        d / r
    }
}

/// Minimum distance from `position` to any of `self_positions`.
fn min_distance(position: Vector3, self_positions: &[Vector3]) -> f32 {
    self_positions
        .iter()
        .map(|sp| position.dist(*sp))
        .fold(f32::MAX, f32::min)
}

impl StorageManager {
    pub fn new(max_capacity_bytes: u64) -> Self {
        Self {
            max_capacity_bytes,
            current_usage: 0,
            blocks_meta: HashMap::new(),
            lru_index: BTreeMap::new(),
            clock: 0,
            pinned: HashSet::new(),
        }
    }

    /// Replaces the pin protection set wholesale (SPEC-18). Called by
    /// `StorageEngine` whenever its `PinRegistry` changes (pin/unpin, or the
    /// initial lazy load), rather than incrementally, since the registry is
    /// the source of truth and recomputing its full CID union is cheap.
    pub fn set_pinned(&mut self, pinned: HashSet<String>) {
        self.pinned = pinned;
    }

    pub fn track_block(&mut self, cid: &str, size: u64, position: Option<Vector3>) -> bool {
        self.clock += 1;
        if let Some(meta) = self.blocks_meta.get_mut(cid) {
            self.lru_index.remove(&meta.last_accessed);
            self.lru_index.insert(self.clock, cid.to_string());
            meta.last_accessed = self.clock;
            if position.is_some() {
                meta.position = position;
            }
            return false;
        }

        self.blocks_meta.insert(
            cid.to_string(),
            BlockMeta {
                last_accessed: self.clock,
                size,
                position,
            },
        );
        self.lru_index.insert(self.clock, cid.to_string());
        self.current_usage += size;
        true
    }

    pub fn touch(&mut self, cid: &str) {
        if let Some(meta) = self.blocks_meta.get_mut(cid) {
            self.clock += 1;
            self.lru_index.remove(&meta.last_accessed);
            self.lru_index.insert(self.clock, cid.to_string());
            meta.last_accessed = self.clock;
        }
    }

    pub fn untrack_block(&mut self, cid: &str) -> Option<u64> {
        if let Some(meta) = self.blocks_meta.remove(cid) {
            self.lru_index.remove(&meta.last_accessed);
            self.current_usage = self.current_usage.saturating_sub(meta.size);
            Some(meta.size)
        } else {
            None
        }
    }

    pub fn eviction_candidates(&self) -> Vec<String> {
        if self.current_usage <= self.max_capacity_bytes {
            return Vec::new();
        }

        let mut remaining = self.current_usage - self.max_capacity_bytes;
        let mut victims = Vec::new();

        for (_, cid) in self.lru_index.iter() {
            // Pinned blocks (SPEC-18) are never victims; skip past them
            // rather than breaking, so a pinned entry doesn't block older
            // unpinned entries behind it in LRU order from being considered.
            if self.pinned.contains(cid) {
                continue;
            }
            if let Some(meta) = self.blocks_meta.get(cid) {
                victims.push(cid.clone());
                remaining = remaining.saturating_sub(meta.size);
                if remaining == 0 {
                    break;
                }
            }
        }

        victims
    }

    /// Spatially-weighted victim selection: prefers evicting blocks that are
    /// both old *and* far from every position in `self_positions` (distance
    /// weighted by `retention_radius`; see SPEC-16 for the score formula).
    /// Blocks with no tagged `position` are scored as if `d == 0` (pure age).
    ///
    /// Mirrors `eviction_candidates()`'s capacity check, but adds hysteresis:
    /// once over capacity, victims are selected down to 95% of
    /// `max_capacity_bytes` rather than just back down to 100%, so that a
    /// capacity-exceeding write burst doesn't re-trigger a full metadata scan
    /// on every single chunk.
    pub fn spatial_eviction_candidates(
        &self,
        self_positions: &[Vector3],
        retention_radius: f32,
    ) -> Vec<String> {
        if self.current_usage <= self.max_capacity_bytes {
            return Vec::new();
        }

        let target = (self.max_capacity_bytes as f64 * 0.95) as u64;
        let mut remaining = self.current_usage.saturating_sub(target);
        if remaining == 0 {
            return Vec::new();
        }

        let scored: Vec<ScoredBlock> = self
            .blocks_meta
            .iter()
            // Pinned blocks (SPEC-18) are excluded from scoring entirely,
            // not just skipped when popped -- they must never occupy a slot
            // in the victim heap regardless of how "evictable" their score
            // would otherwise look.
            .filter(|(cid, _)| !self.pinned.contains(*cid))
            .map(|(cid, meta)| {
                let age = (self.clock - meta.last_accessed + 1) as f64;
                let coeff = match meta.position {
                    Some(position) if !self_positions.is_empty() => {
                        let d = min_distance(position, self_positions);
                        let ratio = distance_ratio(d, retention_radius);
                        1.0 + (ratio * ratio) as f64
                    }
                    _ => 1.0,
                };
                ScoredBlock {
                    effective_age: age * coeff,
                    size: meta.size,
                    cid: cid.clone(),
                }
            })
            .collect();

        // Heapify is O(n); each of the (bounded) pops below is O(log n), for
        // an overall O(n + k log n) instead of a full O(n log n) sort.
        let mut heap: BinaryHeap<ScoredBlock> = BinaryHeap::from(scored);
        let mut victims = Vec::new();
        while remaining > 0 {
            match heap.pop() {
                Some(entry) => {
                    remaining = remaining.saturating_sub(entry.size);
                    victims.push(entry.cid);
                }
                None => break,
            }
        }
        victims
    }

    /// Blocks eligible for probabilistic decay: only ones with a tagged
    /// `position` (untagged blocks are never decayed) that lie beyond
    /// `retention_radius` from every position in `self_positions`. The roll
    /// is a deterministic hash of `(cid, sweep_counter)` rather than an RNG,
    /// so sweeps are reproducible in tests and avoid pulling in a `rand`
    /// dependency or wall-clock time (unavailable under the wasm target;
    /// see docs/investigation/TROUBLESHOOTING.md).
    pub fn decay_candidates(
        &self,
        self_positions: &[Vector3],
        retention_radius: f32,
        max_probability: f32,
        sweep_counter: u64,
    ) -> Vec<String> {
        if self_positions.is_empty() {
            return Vec::new();
        }

        let mut victims = Vec::new();
        for (cid, meta) in self.blocks_meta.iter() {
            // Pinned blocks (SPEC-18) are never decay-eligible, tagged or not.
            if self.pinned.contains(cid) {
                continue;
            }
            let Some(position) = meta.position else {
                continue;
            };
            let d = min_distance(position, self_positions);
            if d <= retention_radius {
                continue;
            }

            let p = max_probability * ((d - retention_radius) / (3.0 * retention_radius)).min(1.0);
            if Self::deterministic_roll(cid, sweep_counter) < p as f64 {
                victims.push(cid.clone());
            }
        }
        victims
    }

    /// Deterministic pseudo-random roll in `[0, 1]` derived from `(cid,
    /// sweep_counter)`. Not cryptographically meaningful — only needs to be
    /// stable and roughly uniform so `decay_candidates` is reproducible.
    /// The exact 1.0 value (hash == u64::MAX, probability 2^-64) combined
    /// with the strict `< p` comparison can only make eviction marginally
    /// less likely, never evict a block that `p` says to keep.
    fn deterministic_roll(cid: &str, sweep_counter: u64) -> f64 {
        let mut hasher = DefaultHasher::new();
        cid.hash(&mut hasher);
        sweep_counter.hash(&mut hasher);
        (hasher.finish() as f64) / (u64::MAX as f64)
    }

    pub fn current_usage(&self) -> u64 {
        self.current_usage
    }
    pub fn max_capacity(&self) -> u64 {
        self.max_capacity_bytes
    }
    pub fn block_count(&self) -> usize {
        self.blocks_meta.len()
    }

    /// Test-only peek at a block's tagged position, so tests can assert on
    /// auto-tagging without exposing `position` as part of the real API.
    #[cfg(test)]
    pub(crate) fn peek_position(&self, cid: &str) -> Option<Vector3> {
        self.blocks_meta.get(cid).and_then(|meta| meta.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lru_eviction() {
        let mut mgr = StorageManager::new(100);
        mgr.track_block("a", 60, None);
        mgr.track_block("b", 60, None);
        let victims = mgr.eviction_candidates();
        assert_eq!(victims.len(), 1);
        assert_eq!(victims[0], "a");
    }

    #[test]
    fn test_track_block_updates_access_time() {
        let mut mgr = StorageManager::new(15);
        mgr.track_block("a", 10, None);
        mgr.track_block("b", 10, None);

        let first_candidates = mgr.eviction_candidates();
        assert_eq!(first_candidates.len(), 1);
        assert_eq!(first_candidates[0], "a");

        assert!(!mgr.track_block("a", 10, None));
        let second_candidates = mgr.eviction_candidates();
        assert_eq!(second_candidates.len(), 1);
        assert_eq!(second_candidates[0], "b");
    }

    #[test]
    fn test_lru_eviction_large_scale() {
        let n = 1000;
        let mut mgr = StorageManager::new(900);
        for i in 0..n {
            mgr.track_block(&i.to_string(), 1, None);
        }

        let victims = mgr.eviction_candidates();
        assert_eq!(victims.len(), 100);
        assert_eq!(victims[0], "0");
        assert_eq!(victims[99], "99");
    }

    #[test]
    fn test_lru_eviction_respects_touch() {
        let n = 1000;
        let mut mgr = StorageManager::new(900);
        for i in 0..n {
            mgr.track_block(&i.to_string(), 1, None);
        }
        mgr.touch("0");

        let victims = mgr.eviction_candidates();
        assert_eq!(victims.len(), 100);
        assert_ne!(victims[0], "0");
        assert!(victims.iter().all(|v| v != "0"));
    }

    #[test]
    fn test_lru_eviction_perf() {
        let n = 100_000;
        let mut mgr = StorageManager::new((n as u64) - 10);
        for i in 0..n {
            mgr.track_block(&i.to_string(), 1, None);
        }

        let iters = 50;
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let _ = mgr.eviction_candidates();
        }
        let duration = start.elapsed();
        let avg_ms = duration.as_secs_f64() * 1000.0 / (iters as f64);
        println!(
            "LRU eviction candidates: {}iters => total {:?}, avg {:.3}ms",
            iters, duration, avg_ms
        );

        // Note: this is a lightweight regression threshold to catch big regressions on CI.
        // Real-world perf expected < 100ms in most environments and might vary under load.
        assert!(
            avg_ms < 200.0,
            "eviction_candidates too slow: avg {:.3}ms",
            avg_ms
        );
    }

    #[test]
    fn test_spatial_eviction_prefers_farther_block_at_same_age() {
        let mut mgr = StorageManager::new(100);
        // Same size and tracked back-to-back (near-identical age), but "far"
        // sits much farther from the self position than "near".
        mgr.track_block("near", 60, Some(Vector3::new(1.0, 0.0, 0.0)));
        mgr.track_block("far", 60, Some(Vector3::new(1000.0, 0.0, 0.0)));

        let victims = mgr.spatial_eviction_candidates(&[Vector3::new(0.0, 0.0, 0.0)], 100.0);
        assert_eq!(victims, vec!["far".to_string()]);
    }

    #[test]
    fn test_spatial_eviction_treats_untagged_block_as_coefficient_one() {
        let mut mgr = StorageManager::new(100);
        // "untagged" has no position (coefficient 1.0, i.e. as if d == 0) and
        // is older, so it must be evicted before "far", which is tagged well
        // beyond the retention radius (large distance coefficient).
        mgr.track_block("untagged", 60, None);
        mgr.track_block("far", 60, Some(Vector3::new(1000.0, 0.0, 0.0)));

        let victims = mgr.spatial_eviction_candidates(&[Vector3::new(0.0, 0.0, 0.0)], 100.0);
        assert_eq!(victims, vec!["far".to_string()]);
    }

    #[test]
    fn test_spatial_eviction_hysteresis_targets_95_percent() {
        let cap = 1000u64;
        let mut mgr = StorageManager::new(cap);
        for i in 0..20 {
            mgr.track_block(&i.to_string(), 60, Some(Vector3::new(1000.0, 0.0, 0.0)));
        }
        // current_usage = 1200, over the 1000 cap.
        let victims = mgr.spatial_eviction_candidates(&[Vector3::new(0.0, 0.0, 0.0)], 100.0);

        let target = (cap as f64 * 0.95) as u64;
        let evicted_bytes = victims.len() as u64 * 60;
        let remaining_usage = 1200u64 - evicted_bytes;
        assert!(
            remaining_usage <= target,
            "expected remaining usage <= 95% target ({target}), got {remaining_usage}"
        );
        // One fewer eviction must leave it back above the target (tight fit
        // check, guards against over-evicting too).
        assert!(
            1200u64 - (evicted_bytes - 60) > target,
            "evicted more than necessary to reach the 95% target"
        );
    }

    #[test]
    fn test_spatial_eviction_uses_min_distance_across_self_positions() {
        let mut mgr = StorageManager::new(100);
        let self_positions = [Vector3::new(0.0, 0.0, 0.0), Vector3::new(1000.0, 0.0, 0.0)];

        // "near_second" sits right on top of self_positions[1] (min distance
        // 0, must be treated like an untagged block), while "far_from_both"
        // is roughly equidistant from both self points and well outside the
        // retention radius either way. Tracked back-to-back so age is nearly
        // identical; only the min-distance computation should decide.
        mgr.track_block("near_second", 60, Some(Vector3::new(1000.0, 0.0, 0.0)));
        mgr.track_block("far_from_both", 60, Some(Vector3::new(500.0, 0.0, 0.0)));

        let victims = mgr.spatial_eviction_candidates(&self_positions, 100.0);
        assert_eq!(
            victims,
            vec!["far_from_both".to_string()],
            "distance must be measured against the nearest self_position, not just self_positions[0]"
        );
    }

    #[test]
    fn test_decay_never_evicts_within_retention_radius() {
        let mut mgr = StorageManager::new(100);
        mgr.track_block("close", 10, Some(Vector3::new(50.0, 0.0, 0.0)));
        let victims = mgr.decay_candidates(&[Vector3::new(0.0, 0.0, 0.0)], 100.0, 1.0, 0);
        assert!(victims.is_empty(), "d <= R must never decay (p=0)");
    }

    #[test]
    fn test_decay_probability_reaches_max_at_4r() {
        let mut mgr = StorageManager::new(100);
        // d == 4R hits the p == max_probability cap (min(1.0, (d-R)/(3R)) == 1.0).
        mgr.track_block("edge", 10, Some(Vector3::new(400.0, 0.0, 0.0)));

        // With max_probability == 1.0, every deterministic roll in [0, 1)
        // must be < p == 1.0, so the block is always selected.
        let victims = mgr.decay_candidates(&[Vector3::new(0.0, 0.0, 0.0)], 100.0, 1.0, 42);
        assert_eq!(victims, vec!["edge".to_string()]);
    }

    #[test]
    fn test_decay_skips_untagged_blocks() {
        let mut mgr = StorageManager::new(100);
        mgr.track_block("untagged", 10, None);
        // Even with max_probability == 1.0 (guaranteed decay for any tagged,
        // out-of-radius block), an untagged block must never appear.
        let victims = mgr.decay_candidates(&[Vector3::new(0.0, 0.0, 0.0)], 1.0, 1.0, 7);
        assert!(victims.is_empty(), "untagged blocks must never decay");
    }

    #[test]
    fn test_decay_roll_is_deterministic_across_calls() {
        let mut mgr = StorageManager::new(100);
        mgr.track_block("a", 10, Some(Vector3::new(500.0, 0.0, 0.0)));
        mgr.track_block("b", 10, Some(Vector3::new(500.0, 0.0, 0.0)));

        let self_positions = [Vector3::new(0.0, 0.0, 0.0)];
        let first = mgr.decay_candidates(&self_positions, 100.0, 0.2, 5);
        let second = mgr.decay_candidates(&self_positions, 100.0, 0.2, 5);
        assert_eq!(
            first, second,
            "same sweep_counter must reproduce the same roll"
        );
    }

    #[test]
    fn test_spatial_eviction_candidates_perf() {
        let n = 100_000;
        let mut mgr = StorageManager::new((n as u64) - 10);
        for i in 0..n {
            let position = Vector3::new((i % 500) as f32, 0.0, 0.0);
            mgr.track_block(&i.to_string(), 1, Some(position));
        }
        let self_positions = [Vector3::new(0.0, 0.0, 0.0)];

        let iters = 50;
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let _ = mgr.spatial_eviction_candidates(&self_positions, 100.0);
        }
        let duration = start.elapsed();
        let avg_ms = duration.as_secs_f64() * 1000.0 / (iters as f64);
        println!(
            "Spatial eviction candidates: {}iters => total {:?}, avg {:.3}ms",
            iters, duration, avg_ms
        );

        assert!(
            avg_ms < 200.0,
            "spatial_eviction_candidates too slow: avg {:.3}ms",
            avg_ms
        );
    }

    #[test]
    fn test_set_pinned_excludes_block_from_lru_eviction_candidates() {
        let mut mgr = StorageManager::new(100);
        mgr.track_block("a", 60, None);
        mgr.track_block("b", 60, None);
        mgr.set_pinned(HashSet::from(["a".to_string()]));

        // Without pinning, "a" (older) would be the sole LRU victim (see
        // test_lru_eviction); pinning it must skip straight to "b" instead
        // of leaving the over-capacity condition unresolved.
        let victims = mgr.eviction_candidates();
        assert_eq!(victims, vec!["b".to_string()]);
    }

    #[test]
    fn test_set_pinned_excludes_block_from_spatial_eviction_candidates() {
        let mut mgr = StorageManager::new(100);
        // "far" would normally be the clear spatial victim (see
        // test_spatial_eviction_prefers_farther_block_at_same_age); pinning
        // it must divert eviction to "near" instead.
        mgr.track_block("near", 60, Some(Vector3::new(1.0, 0.0, 0.0)));
        mgr.track_block("far", 60, Some(Vector3::new(1000.0, 0.0, 0.0)));
        mgr.set_pinned(HashSet::from(["far".to_string()]));

        let victims = mgr.spatial_eviction_candidates(&[Vector3::new(0.0, 0.0, 0.0)], 100.0);
        assert_eq!(victims, vec!["near".to_string()]);
    }

    #[test]
    fn test_set_pinned_excludes_block_from_decay_candidates() {
        let mut mgr = StorageManager::new(100);
        // Same setup as test_decay_probability_reaches_max_at_4r (guaranteed
        // decay for an unpinned block at d == 4R with max_probability 1.0),
        // but pinned this time.
        mgr.track_block("edge", 10, Some(Vector3::new(400.0, 0.0, 0.0)));
        mgr.set_pinned(HashSet::from(["edge".to_string()]));

        let victims = mgr.decay_candidates(&[Vector3::new(0.0, 0.0, 0.0)], 100.0, 1.0, 42);
        assert!(victims.is_empty(), "pinned blocks must never decay");
    }
}
