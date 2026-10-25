use async_trait::async_trait;
pub use mistlib_core::storage::protocol::{
    build_have_chunk_message, build_have_message, build_have_status_message, build_query_message,
    parse_have_chunk_message, parse_have_message, parse_have_status_message, parse_query_message,
    parse_want_message, WantRegistry, HAVE_CHUNK_SIZE, MSG_HAVE, MSG_HAVE_CHUNK, MSG_HAVE_STATUS,
    MSG_QUERY, MSG_WANT,
};
use mistlib_core::storage::resolver::{NetworkPeerResolver, ResolverTimings, TransportSource};
use mistlib_core::storage::PeerResolver;
use mistlib_core::transport::Transport;
use std::sync::Arc;

struct WasmSessionTransports;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl TransportSource for WasmSessionTransports {
    async fn transports(&self) -> Vec<Arc<dyn Transport>> {
        // Storage is process-wide but content-addressed blocks aren't
        // room-scoped, so fan out across every joined room's transport,
        // snapshotted on every call (no single captured transport --
        // multi-room contract point 10).
        crate::app::all_session_transports()
    }

    async fn session_transports(&self) -> Vec<(String, Arc<dyn Transport>)> {
        crate::app::all_storage_session_transports()
    }
}

pub struct WasmPeerResolver {
    inner: NetworkPeerResolver,
}

impl WasmPeerResolver {
    pub fn new(registry: WantRegistry) -> Self {
        Self {
            inner: NetworkPeerResolver::new_with_transfer(
                Arc::new(WasmSessionTransports),
                Arc::new(crate::runtime::WasmRuntime),
                registry,
                ResolverTimings::default(),
                crate::app::base_config().storage.transfer,
            ),
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl PeerResolver for WasmPeerResolver {
    async fn resolve_block_result(
        &self,
        cid: &str,
    ) -> mistlib_core::error::Result<Option<Vec<u8>>> {
        self.inner.resolve_block_result(cid).await
    }

    async fn prefetch_providers(&self, cids: &[String]) {
        self.inner.prefetch_providers(cids).await;
    }

    async fn resolve_block(&self, cid: &str) -> Option<Vec<u8>> {
        self.inner.resolve_block(cid).await
    }
}
