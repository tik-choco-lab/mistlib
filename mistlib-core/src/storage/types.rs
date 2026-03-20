use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    clock: u64,
}

struct BlockMeta {
    last_accessed: u64,
    size: u64,
}

impl StorageManager {
    pub fn new(max_capacity_bytes: u64) -> Self {
        Self {
            max_capacity_bytes,
            current_usage: 0,
            blocks_meta: HashMap::new(),
            clock: 0,
        }
    }

    pub fn track_block(&mut self, cid: &str, size: u64) -> bool {
        if let Some(meta) = self.blocks_meta.get_mut(cid) {
            self.clock += 1;
            meta.last_accessed = self.clock;
            return false;
        }

        self.clock += 1;
        self.blocks_meta.insert(
            cid.to_string(),
            BlockMeta {
                last_accessed: self.clock,
                size,
            },
        );
        self.current_usage += size;
        true
    }

    pub fn touch(&mut self, cid: &str) {
        if let Some(meta) = self.blocks_meta.get_mut(cid) {
            self.clock += 1;
            meta.last_accessed = self.clock;
        }
    }

    pub fn untrack_block(&mut self, cid: &str) -> Option<u64> {
        if let Some(meta) = self.blocks_meta.remove(cid) {
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

        let mut entries: Vec<_> = self.blocks_meta.iter().collect();
        entries.sort_by_key(|(_, m)| m.last_accessed);

        for (cid, meta) in entries {
            victims.push(cid.clone());
            remaining = remaining.saturating_sub(meta.size);
            if remaining == 0 {
                break;
            }
        }

        victims
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lru_eviction() {
        let mut mgr = StorageManager::new(100);
        mgr.track_block("a", 60);
        mgr.track_block("b", 60);
        let victims = mgr.eviction_candidates();
        assert_eq!(victims.len(), 1);
        assert_eq!(victims[0], "a");
    }

    #[test]
    fn test_track_block_updates_access_time() {
        let mut mgr = StorageManager::new(15);
        mgr.track_block("a", 10);
        mgr.track_block("b", 10);

        let first_candidates = mgr.eviction_candidates();
        assert_eq!(first_candidates.len(), 1);
        assert_eq!(first_candidates[0], "a");

        assert!(!mgr.track_block("a", 10));
        let second_candidates = mgr.eviction_candidates();
        assert_eq!(second_candidates.len(), 1);
        assert_eq!(second_candidates[0], "b");
    }

    #[test]
    fn test_lru_eviction_large_scale() {
        let n = 1000;
        let mut mgr = StorageManager::new(900);
        for i in 0..n {
            mgr.track_block(&i.to_string(), 1);
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
            mgr.track_block(&i.to_string(), 1);
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
            mgr.track_block(&i.to_string(), 1);
        }

        let iters = 50;
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let _ = mgr.eviction_candidates();
        }
        let duration = start.elapsed();
        let avg_ms = duration.as_secs_f64() * 1000.0 / (iters as f64);
        println!("LRU eviction candidates: {}iters => total {:?}, avg {:.3}ms", iters, duration, avg_ms);

        assert!(avg_ms < 200.0, "eviction_candidates too slow: avg {:.3}ms", avg_ms);
    }
}

