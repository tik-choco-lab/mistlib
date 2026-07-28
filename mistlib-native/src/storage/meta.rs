//! Filesystem-backed `MetaStore` (SPEC-17): one file per key under
//! `<base_dir>/meta/`, sibling to the CID block files. Key validation/size
//! limits are enforced at the API boundary (core `storage::meta` helpers);
//! the hex `encode_meta_key` filename is filesystem-safe by construction, so
//! no path-traversal check beyond it is needed here.

use async_trait::async_trait;
use mistlib_core::error::{MistError, Result};
use mistlib_core::storage::meta::encode_meta_key;
use mistlib_core::storage::MetaStore;
use std::path::{Path, PathBuf};
use tokio::fs;

pub struct NativeMetaStore {
    base_dir: PathBuf,
}

impl NativeMetaStore {
    pub async fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let base_dir = path.as_ref().to_path_buf();
        fs::create_dir_all(&base_dir)
            .await
            .map_err(|e| MistError::Internal(format!("Failed to create meta dir: {}", e)))?;
        Ok(Self { base_dir })
    }

    fn key_path(&self, key: &str) -> PathBuf {
        self.base_dir.join(encode_meta_key(key))
    }
}

#[async_trait]
impl MetaStore for NativeMetaStore {
    async fn set(&self, key: &str, data: &[u8]) -> Result<()> {
        fs::write(self.key_path(key), data)
            .await
            .map_err(|e| MistError::Internal(format!("Failed to write meta '{}': {}", key, e)))
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match fs::read(self.key_path(key)).await {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(MistError::Internal(format!(
                "Failed to read meta '{}': {}",
                key, e
            ))),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        match fs::remove_file(self.key_path(key)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(MistError::Internal(format!(
                "Failed to delete meta '{}': {}",
                key, e
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_store() -> (NativeMetaStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = NativeMetaStore::new(dir.path().join("meta"))
            .await
            .expect("meta store");
        (store, dir)
    }

    #[tokio::test]
    async fn test_set_get_roundtrip_and_overwrite() {
        let (store, _dir) = temp_store().await;
        store.set("pins", b"v1").await.unwrap();
        assert_eq!(
            store.get("pins").await.unwrap().as_deref(),
            Some(&b"v1"[..])
        );
        // Overwrite (last-write-wins, shorter content must truncate).
        store.set("pins", b"2").await.unwrap();
        assert_eq!(store.get("pins").await.unwrap().as_deref(), Some(&b"2"[..]));
    }

    #[tokio::test]
    async fn test_get_missing_is_none_and_delete_is_idempotent() {
        let (store, _dir) = temp_store().await;
        assert_eq!(store.get("missing").await.unwrap(), None);
        store.delete("missing").await.unwrap();
        store.set("k", b"x").await.unwrap();
        store.delete("k").await.unwrap();
        assert_eq!(store.get("k").await.unwrap(), None);
        store.delete("k").await.unwrap();
    }

    #[tokio::test]
    async fn test_pathological_keys_do_not_escape_meta_dir() {
        let (store, dir) = temp_store().await;
        store.set("../escape", b"x").await.unwrap();
        assert_eq!(
            store.get("../escape").await.unwrap().as_deref(),
            Some(&b"x"[..])
        );
        // Nothing may exist outside the meta dir root.
        assert!(!dir.path().join("escape").exists());
        assert!(!dir.path().parent().unwrap().join("escape").exists());
    }
}
