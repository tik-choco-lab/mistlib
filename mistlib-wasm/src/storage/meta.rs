//! OPFS-backed `MetaStore` (SPEC-17): one file per key under the top-level
//! `mistlib-meta` directory, separate from the `mistlib-blocks` CAS
//! directory. Key validation/size limits are enforced at the API boundary
//! (core `storage::meta` helpers); this layer only does encoded-filename IO.

use async_trait::async_trait;
use mistlib_core::error::Result;
use mistlib_core::storage::meta::encode_meta_key;
use mistlib_core::storage::MetaStore;

use super::opfs::{get_dir, read_file, remove_file, write_file};

const META_DIR: &str = "mistlib-meta";

pub struct WasmMetaStore;

#[async_trait(?Send)]
impl MetaStore for WasmMetaStore {
    async fn set(&self, key: &str, data: &[u8]) -> Result<()> {
        let dir = get_dir(META_DIR).await?;
        write_file(&dir, &encode_meta_key(key), data).await
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let dir = get_dir(META_DIR).await?;
        read_file(&dir, &encode_meta_key(key)).await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let dir = get_dir(META_DIR).await?;
        remove_file(&dir, &encode_meta_key(key)).await
    }
}
