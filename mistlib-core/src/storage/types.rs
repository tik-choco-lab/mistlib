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
        if self.blocks_meta.contains_key(cid) {
            self.touch(cid);
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
        let mut entries: Vec<_> = self.blocks_meta.iter().collect();
        entries.sort_by_key(|(_, meta)| meta.last_accessed);
        let mut to_free = self.current_usage - self.max_capacity_bytes;
        let mut victims = Vec::new();
        for (cid, meta) in entries {
            if to_free == 0 {
                break;
            }
            victims.push(cid.clone());
            to_free = to_free.saturating_sub(meta.size);
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
}
