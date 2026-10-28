//! Rust host hooks for serving and fetching individual blocks without raw frames.

use super::WANT_REGISTRY;
use crate::{MistError, Result};
use mistlib_core::storage::cid::{cid_codec, verify_cid};
use mistlib_core::storage::protocol::{build_query_message, build_want_message};
use mistlib_core::types::{DeliveryMethod, NodeId};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

#[async_trait::async_trait]
pub trait BlockProvider: Send + Sync + 'static {
    /// A peer requested this CID in this room. Enforce access control here;
    /// returning None falls through to the next provider and then the store.
    async fn get_block(&self, room_id: &str, from: &NodeId, cid: &str) -> Option<Vec<u8>>;

    /// Answer QUERY. Override to avoid loading the block for discovery.
    async fn has_block(&self, room_id: &str, from: &NodeId, cid: &str) -> bool {
        self.get_block(room_id, from, cid).await.is_some()
    }
}

type Providers = Vec<(Arc<()>, Arc<dyn BlockProvider>)>;
static PROVIDERS: LazyLock<Mutex<Providers>> = LazyLock::new(Mutex::default);

/// Register in priority order. Keep the returned handle alive while serving.
#[must_use = "Keep the handle alive to keep the provider registered"]
pub fn register_block_provider(provider: Arc<dyn BlockProvider>) -> BlockProviderHandle {
    let id = Arc::new(());
    PROVIDERS.lock().unwrap().push((id.clone(), provider));
    BlockProviderHandle { id }
}

/// Dropping the handle unregisters its provider. Calls already in progress may
/// finish using their snapshot; no registry lock is held while calling hosts.
#[must_use]
pub struct BlockProviderHandle {
    id: Arc<()>,
}

impl Drop for BlockProviderHandle {
    fn drop(&mut self) {
        let removed = {
            let mut providers = PROVIDERS.lock().unwrap();
            providers
                .iter()
                .position(|(id, _)| Arc::ptr_eq(id, &self.id))
                .map(|index| providers.remove(index))
        };
        // Host destructors may register/unregister other providers.
        drop(removed);
    }
}

fn snapshot() -> Vec<Arc<dyn BlockProvider>> {
    PROVIDERS
        .lock()
        .unwrap()
        .iter()
        .map(|(_, p)| p.clone())
        .collect()
}

pub(super) async fn get_block(room: &str, from: &NodeId, cid: &str) -> Option<Vec<u8>> {
    for provider in snapshot() {
        if let Some(data) = provider.get_block(room, from, cid).await {
            return Some(data);
        }
    }
    None
}

pub(super) async fn has_block(room: &str, from: &NodeId, cid: &str) -> bool {
    for provider in snapshot() {
        if provider.has_block(room, from, cid).await {
            return true;
        }
    }
    false
}

pub struct FetchOptions {
    /// Some: send WANT only to this peer without discovery.
    /// None: discover with QUERY/HAVE_STATUS in this room first.
    pub peer: Option<NodeId>,
    /// Maximum received payload bytes, checked before copying/reassembly.
    /// The registry's existing block size ceiling also applies.
    pub max_bytes: usize,
    /// Bounds the entire operation, including discovery and send admission.
    pub timeout: Duration,
}

// Declared before receivers, so their drop closes registrations before pruning.
struct PruneOnDrop;
impl Drop for PruneOnDrop {
    fn drop(&mut self) {
        WANT_REGISTRY.cancel_closed();
    }
}

/// Fetch one block through the room session, verify its embedded codec/hash,
/// and return it without writing to the STORAGE cache. Currently supports
/// CIDv1/base32lower with SHA2-256; unsupported hashes return an error.
pub async fn fetch_block(room_id: &str, cid: &str, opts: FetchOptions) -> Result<Vec<u8>> {
    let codec = cid_codec(cid)
        .ok_or_else(|| MistError::Config("Invalid or unsupported block CID".into()))?;
    let ctx = crate::engine::ENGINE
        .get_session(room_id)
        .await
        .ok_or_else(|| MistError::Config(format!("No active session for room {room_id}")))?;
    let _prune = PruneOnDrop;
    let transfer = async {
        let peer = match opts.peer {
            Some(peer) => peer,
            None => {
                let reply = WANT_REGISTRY.register_peer_notifier_in(room_id, cid);
                ctx.transport
                    .broadcast(
                        build_query_message(cid).into(),
                        DeliveryMethod::ReliableOrdered,
                    )
                    .await?;
                reply
                    .await
                    .map_err(|_| MistError::Network("Block discovery registration closed".into()))?
            }
        };
        let (attempt, mut exceeded) =
            WANT_REGISTRY.register_bounded_attempt(cid, peer.clone(), opts.max_bytes);
        if !WANT_REGISTRY.track_path(attempt.id, room_id) {
            return Err(MistError::Network(
                "Block fetch registration rejected".into(),
            ));
        }
        // Register and attach the sole authorized room before awaiting send.
        // Oversized responses abort even while send admission is still pending.
        let receive = async {
            ctx.transport
                .send(
                    &peer,
                    build_want_message(cid).into(),
                    DeliveryMethod::ReliableOrdered,
                )
                .await?;
            let data = attempt
                .data
                .await
                .map_err(|_| MistError::Network("Block fetch registration closed".into()))?;
            if !verify_cid(cid, &data, codec) {
                return Err(MistError::Network("Block CID/hash mismatch".into()));
            }
            Ok(data)
        };
        tokio::select! {
            biased;
            Ok(size) = &mut exceeded => Err(MistError::Network(format!("Block exceeds receive byte limit (observed {} bytes)", size))),
            result = receive => result,
        }
    };
    tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => Err(MistError::Network("Block fetch room session closed".into())),
        result = tokio::time::timeout(opts.timeout, transfer) => {
            result.map_err(|_| MistError::Network("Block fetch timeout".into()))?
        }
    }
}
