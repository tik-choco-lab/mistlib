use async_trait::async_trait;
use mistlib_core::error::{MistError, Result};
use mistlib_core::storage::is_valid_cid;
use mistlib_core::storage::BlockStore;
use std::path::{Path, PathBuf};
use tokio::fs;

pub struct NativeBlockStore {
    base_dir: PathBuf,
}

impl NativeBlockStore {
    pub async fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let base_dir = path.as_ref().to_path_buf();
        fs::create_dir_all(&base_dir)
            .await
            .map_err(|e| MistError::Internal(format!("Failed to create storage dir: {}", e)))?;
        Ok(Self { base_dir })
    }

    fn cid_path(&self, cid: &str) -> PathBuf {
        self.base_dir.join(cid)
    }

    /// Rejects any CID that isn't a well-formed CIDv1 before it is joined to
    /// `base_dir`. Untrusted peers supply CIDs over the storage protocol, and
    /// `is_valid_cid` guarantees the string contains no path separator, `..`,
    /// or drive/root prefix — so the join can't escape the block directory.
    fn safe_cid_path(&self, cid: &str) -> Option<PathBuf> {
        if is_valid_cid(cid) {
            Some(self.cid_path(cid))
        } else {
            None
        }
    }
}

#[async_trait]
impl BlockStore for NativeBlockStore {
    async fn store_block(&self, cid: &str, data: &[u8]) -> Result<()> {
        let path = self.safe_cid_path(cid).ok_or_else(|| {
            MistError::Internal(format!("Refusing to store block with invalid CID: {}", cid))
        })?;
        fs::write(&path, data)
            .await
            .map_err(|e| MistError::Internal(format!("Failed to write block {}: {}", cid, e)))?;
        Ok(())
    }

    async fn load_block(&self, cid: &str) -> Result<Option<Vec<u8>>> {
        let path = match self.safe_cid_path(cid) {
            Some(path) => path,
            None => return Ok(None),
        };
        match fs::read(&path).await {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(MistError::Internal(format!(
                "Failed to read block {}: {}",
                cid, e
            ))),
        }
    }

    async fn delete_block(&self, cid: &str) -> Result<()> {
        let path = match self.safe_cid_path(cid) {
            Some(path) => path,
            None => return Ok(()),
        };
        match fs::remove_file(&path).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(MistError::Internal(format!(
                "Failed to delete block {}: {}",
                cid, e
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mistlib_core::storage::cid::MULTICODEC_RAW;
    use mistlib_core::storage::compute_cid;

    fn unique_temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mistlib_native_fs_test_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn load_block_rejects_path_traversal_and_absolute_cids() {
        let root = unique_temp_dir("traversal");
        let base_dir = root.join("blocks");
        fs::create_dir_all(&base_dir).await.unwrap();

        // Place a "secret" file outside of base_dir that a traversal CID
        // would target if validation were missing.
        let secret_path = root.join("secret.txt");
        fs::write(&secret_path, b"top secret contents")
            .await
            .unwrap();

        let store = NativeBlockStore { base_dir };

        // Relative traversal CID must not escape base_dir.
        let traversal_cid = "../secret.txt";
        let result = store.load_block(traversal_cid).await.unwrap();
        assert!(
            result.is_none(),
            "traversal CID must not read a file outside base_dir"
        );

        // Absolute path CID must also be rejected.
        let absolute_cid = secret_path.to_string_lossy().to_string();
        let result = store.load_block(&absolute_cid).await.unwrap();
        assert!(
            result.is_none(),
            "absolute path CID must not read a file outside base_dir"
        );

        fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn store_block_rejects_invalid_cid() {
        let root = unique_temp_dir("store_invalid");
        fs::create_dir_all(&root).await.unwrap();
        let store = NativeBlockStore {
            base_dir: root.clone(),
        };

        let result = store.store_block("../escape", b"data").await;
        assert!(result.is_err(), "invalid CID must be rejected on store");

        fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn valid_cid_round_trips() {
        let root = unique_temp_dir("round_trip");
        fs::create_dir_all(&root).await.unwrap();
        let store = NativeBlockStore {
            base_dir: root.clone(),
        };

        let data = b"hi";
        let cid = compute_cid(data, MULTICODEC_RAW);
        assert!(is_valid_cid(&cid));

        store.store_block(&cid, data).await.unwrap();
        let loaded = store.load_block(&cid).await.unwrap();
        assert_eq!(loaded.as_deref(), Some(&data[..]));

        fs::remove_dir_all(&root).await.ok();
    }
}
