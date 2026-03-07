use async_trait::async_trait;
use mistlib_core::error::{MistError, Result};
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
}

#[async_trait]
impl BlockStore for NativeBlockStore {
    async fn store_block(&self, cid: &str, data: &[u8]) -> Result<()> {
        let path = self.cid_path(cid);
        fs::write(&path, data)
            .await
            .map_err(|e| MistError::Internal(format!("Failed to write block {}: {}", cid, e)))?;
        Ok(())
    }

    async fn load_block(&self, cid: &str) -> Result<Option<Vec<u8>>> {
        let path = self.cid_path(cid);
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
        let path = self.cid_path(cid);
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
