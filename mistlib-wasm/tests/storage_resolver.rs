// Compile the actual wasm adapter on the host and in Node, replacing only its
// session lookup. This catches accidentally inheriting PeerResolver's no-op
// prefetch/default error method without needing a browser room or OPFS.
use async_trait::async_trait;
use mistlib_core::error::Result;
use mistlib_core::runtime::{AsyncRuntime, BoxTask};
use mistlib_core::transport::{NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::sync::{Arc, Mutex};

mod app {
    use super::*;
    pub fn base_config() -> mistlib_core::config::Config {
        mistlib_core::config::Config::default()
    }
    pub fn all_session_transports() -> Vec<Arc<dyn Transport>> {
        Vec::new()
    }
    pub fn all_storage_session_transports() -> Vec<(String, Arc<dyn Transport>)> {
        Vec::new()
    }
}

mod runtime {
    use super::*;
    pub struct WasmRuntime;
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    impl AsyncRuntime for WasmRuntime {
        fn spawn(&self, _: BoxTask) {
            panic!("prefetch should not spawn");
        }
        async fn sleep(&self, _: web_time::Duration) {
            panic!("prefetch should not wait");
        }
    }
}

#[allow(dead_code, unused_imports)]
mod adapter {
    include!("../src/storage/resolver.rs");

    struct Queries(std::sync::Mutex<Vec<String>>);
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    impl Transport for Queries {
        async fn start(&self, _: Arc<dyn super::NetworkEventHandler>) -> super::Result<()> {
            Ok(())
        }
        async fn send(
            &self,
            _: &super::NodeId,
            _: bytes::Bytes,
            _: super::DeliveryMethod,
        ) -> super::Result<()> {
            panic!("prefetch must not send WANTs");
        }
        async fn broadcast(
            &self,
            data: bytes::Bytes,
            _: super::DeliveryMethod,
        ) -> super::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(parse_query_message(&data).unwrap());
            Ok(())
        }
        fn get_connection_state(&self, _: &super::NodeId) -> super::ConnectionState {
            super::ConnectionState::Connected
        }
        async fn connect(&self, _: &super::NodeId) -> super::Result<()> {
            Ok(())
        }
        async fn disconnect(&self, _: &super::NodeId) -> super::Result<()> {
            Ok(())
        }
        fn get_connected_nodes(&self) -> Vec<super::NodeId> {
            Vec::new()
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn wrapper_prefetches_all_five_missing_blocks_before_resolving() {
        let queries = Arc::new(Queries(super::Mutex::new(Vec::new())));
        let registry = WantRegistry::new();
        let resolver = WasmPeerResolver {
            inner: NetworkPeerResolver::new(
                Arc::new(mistlib_core::storage::resolver::FixedTransportSource(vec![
                    queries.clone(),
                ])),
                Arc::new(crate::runtime::WasmRuntime),
                registry,
                ResolverTimings::default(),
            ),
        };
        let cids: Vec<_> = (0..5)
            .map(|i| {
                mistlib_core::storage::cid::compute_cid(
                    &[i],
                    mistlib_core::storage::cid::MULTICODEC_RAW,
                )
            })
            .collect();
        futures::executor::block_on(resolver.prefetch_providers(&cids));
        assert_eq!(*queries.0.lock().unwrap(), cids);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn wrapper_preserves_discovery_timeout_context() {
        struct ExpiredDeadline;
        #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
        #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
        impl super::AsyncRuntime for ExpiredDeadline {
            fn spawn(&self, _: super::BoxTask) {
                panic!("no background discovery");
            }
            async fn sleep(&self, _: web_time::Duration) {}
        }
        let resolver = WasmPeerResolver {
            inner: NetworkPeerResolver::new(
                Arc::new(mistlib_core::storage::resolver::FixedTransportSource(
                    Vec::new(),
                )),
                Arc::new(ExpiredDeadline),
                WantRegistry::new(),
                ResolverTimings::default(),
            ),
        };
        let error =
            futures::executor::block_on(resolver.resolve_block_result("missing")).unwrap_err();
        assert!(
            matches!(error, mistlib_core::error::MistError::Network(message) if message.contains("transport timeout"))
        );
    }
}
