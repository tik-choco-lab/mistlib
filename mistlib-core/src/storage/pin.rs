//! Pin registry (SPEC-18): tracks which root CIDs must never be evicted, and
//! (transitively) which chunk CIDs they protect.
//!
//! The registry itself is a pure data structure with JSON (de)serialization;
//! `StorageEngine` (`engine.rs`) is responsible for persisting it at the
//! `MetaStore` key [`PIN_REGISTRY_META_KEY`] and reflecting its protected set
//! into `StorageManager::set_pinned`.

use crate::error::{MistError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Reserved `MetaStore` key the registry is persisted under.
pub const PIN_REGISTRY_META_KEY: &str = "pins";

/// Registry schema version. Bumping this without a migration path is a
/// deliberate breaking change -- see `from_json`.
const REGISTRY_VERSION: u32 = 1;

/// `{ "v": 1, "roots": { "<root_cid>": ["<block_cid>", ...] } }`. Each
/// `roots` entry lists every CID a pinned root protects: the root (manifest)
/// CID itself plus every chunk CID its manifest enumerates. Storing the
/// expanded list (rather than re-resolving the manifest on every query)
/// keeps `pinned_cids()` a pure in-memory set union, with no I/O or manifest
/// decoding on the eviction hot path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinRegistry {
    v: u32,
    roots: HashMap<String, Vec<String>>,
}

impl PinRegistry {
    pub fn new() -> Self {
        Self {
            v: REGISTRY_VERSION,
            roots: HashMap::new(),
        }
    }

    /// Deserializes a registry previously written by `to_json`. A parse
    /// failure or version mismatch is an `Err`, never a silently-empty
    /// registry: the caller (engine) must not mistake "couldn't read this"
    /// for "nothing is pinned" and hand a fully-unprotected set to the next
    /// eviction sweep. Unknown fields are ignored by serde by default,
    /// giving forward compatibility for additive future versions.
    pub fn from_json(data: &[u8]) -> Result<Self> {
        let registry: Self = serde_json::from_slice(data)?;
        if registry.v != REGISTRY_VERSION {
            return Err(MistError::Serialization(format!(
                "pin registry version {} unsupported (expected {})",
                registry.v, REGISTRY_VERSION
            )));
        }
        Ok(registry)
    }

    pub fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(MistError::from)
    }

    /// Whether `root_cid` itself has a pin entry (not whether some CID is
    /// merely *referenced* by one -- see `pinned_cids` for the protection
    /// set eviction actually consults).
    pub fn is_pinned(&self, root_cid: &str) -> bool {
        self.roots.contains_key(root_cid)
    }

    /// Adds or replaces the pin entry for `root_cid`. `cids` should be the
    /// root CID itself plus every chunk CID its manifest references.
    /// Idempotent: pinning an already-pinned root just overwrites its entry.
    pub fn pin(&mut self, root_cid: &str, cids: Vec<String>) {
        self.roots.insert(root_cid.to_string(), cids);
    }

    /// Removes `root_cid`'s pin entry. Idempotent: unpinning a root with no
    /// entry (never pinned, or already unpinned) is a no-op.
    pub fn unpin(&mut self, root_cid: &str) {
        self.roots.remove(root_cid);
    }

    /// The eviction-protected set: the union of every pinned root's
    /// referenced CIDs. CAS dedup means a chunk can be referenced by more
    /// than one root, so protection is "referenced by *any* pinned root",
    /// not tied to a single owning root.
    pub fn pinned_cids(&self) -> HashSet<String> {
        self.roots.values().flatten().cloned().collect()
    }
}

impl Default for PinRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_round_trip() {
        let mut reg = PinRegistry::new();
        reg.pin(
            "root-a",
            vec!["root-a".into(), "chunk-1".into(), "chunk-2".into()],
        );
        reg.pin("root-b", vec!["root-b".into(), "chunk-2".into()]);

        let bytes = reg.to_json().unwrap();
        let restored = PinRegistry::from_json(&bytes).unwrap();

        assert!(restored.is_pinned("root-a"));
        assert!(restored.is_pinned("root-b"));
        assert_eq!(
            restored.pinned_cids(),
            HashSet::from([
                "root-a".to_string(),
                "root-b".to_string(),
                "chunk-1".to_string(),
                "chunk-2".to_string(),
            ])
        );
    }

    #[test]
    fn test_from_json_rejects_garbage() {
        assert!(PinRegistry::from_json(b"not json at all").is_err());
    }

    #[test]
    fn test_from_json_rejects_version_mismatch() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "v": 999,
            "roots": {}
        }))
        .unwrap();
        assert!(PinRegistry::from_json(&bytes).is_err());
    }

    #[test]
    fn test_from_json_ignores_unknown_fields() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "v": 1,
            "roots": {"root-a": ["root-a"]},
            "future_field": "ignored"
        }))
        .unwrap();
        let restored = PinRegistry::from_json(&bytes).unwrap();
        assert!(restored.is_pinned("root-a"));
    }

    #[test]
    fn test_pinned_cids_is_shared_across_roots_until_last_unpin() {
        let mut reg = PinRegistry::new();
        reg.pin("root-a", vec!["root-a".into(), "shared".into()]);
        reg.pin("root-b", vec!["root-b".into(), "shared".into()]);

        reg.unpin("root-a");
        assert!(
            reg.pinned_cids().contains("shared"),
            "root-b still references the shared chunk"
        );

        reg.unpin("root-b");
        assert!(!reg.pinned_cids().contains("shared"));
    }

    #[test]
    fn test_unpin_is_idempotent() {
        let mut reg = PinRegistry::new();
        reg.unpin("never-pinned"); // must not panic
        assert!(!reg.is_pinned("never-pinned"));
    }
}
