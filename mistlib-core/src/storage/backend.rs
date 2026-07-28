use crate::error::Result;
use crate::types::{HostSendSync, Vector3};
use async_trait::async_trait;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait BlockStore: HostSendSync {
    async fn store_block(&self, cid: &str, data: &[u8]) -> Result<()>;
    async fn load_block(&self, cid: &str) -> Result<Option<Vec<u8>>>;
    async fn delete_block(&self, cid: &str) -> Result<()>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait PeerResolver: HostSendSync {
    async fn resolve_block(&self, cid: &str) -> Option<Vec<u8>>;
}

/// Small mutable named metadata, persisted outside the content-addressed
/// block space (SPEC-17). Unlike blocks, meta entries are mutable
/// (last-write-wins), never chunked, never tracked by `StorageManager`, and
/// never evicted. Keys are validated/encoded by `storage::meta` at the API
/// boundary — implementations may assume the encoded filename is
/// filesystem-safe. `delete` of a missing key is a no-op success.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait MetaStore: HostSendSync {
    async fn set(&self, key: &str, data: &[u8]) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    async fn delete(&self, key: &str) -> Result<()>;
}

/// Supplies the host's current position(s) for spatial storage eviction
/// (SPEC-16). Implemented by native/wasm on top of whatever tracks
/// per-session self-position (e.g. the multi-room session registry); core
/// only consumes it through `StorageEngine`/`P2PStorage`.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait SelfPositionSource: HostSendSync {
    /// Latest self-position of each active session (room). Empty if none is
    /// set (e.g. no session has reported a position yet).
    async fn self_positions(&self) -> Vec<Vector3>;
}
