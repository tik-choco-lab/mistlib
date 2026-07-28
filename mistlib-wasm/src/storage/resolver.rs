use async_trait::async_trait;
pub use mistlib_core::storage::protocol::{
    build_have_chunk_message, build_have_message, build_have_status_message, build_query_message,
    parse_have_chunk_message, parse_have_message, parse_have_status_message, parse_query_message,
    parse_want_message, WantRegistry, HAVE_CHUNK_SIZE, MSG_HAVE, MSG_HAVE_CHUNK, MSG_HAVE_STATUS,
    MSG_QUERY, MSG_WANT,
};
use mistlib_core::storage::PeerResolver;

pub struct WasmPeerResolver {
    registry: WantRegistry,
    timeout_ms: u32,
}

impl WasmPeerResolver {
    pub fn new(registry: WantRegistry, timeout_ms: u32) -> Self {
        Self {
            registry,
            timeout_ms,
        }
    }
}

#[async_trait(?Send)]
impl PeerResolver for WasmPeerResolver {
    async fn resolve_block(&self, cid: &str) -> Option<Vec<u8>> {
        use mistlib_core::types::DeliveryMethod;

        let rx_data = self.registry.register(cid);

        let mut known_peers = self.registry.get_peers(cid);
        if known_peers.is_empty() {
            tracing::debug!("PeerResolver: Discovery phase for {}", cid);
            let rx_peer = self.registry.register_peer_notifier(cid);

            // Storage is process-wide but content-addressed blocks aren't
            // room-scoped, so fan the QUERY out across every joined room's
            // transport, snapshotted right before sending (no single
            // captured transport -- multi-room contract point 10).
            let query_msg = build_query_message(cid);
            for transport in crate::app::all_session_transports() {
                let _ = transport
                    .broadcast(
                        bytes::Bytes::from(query_msg.clone()),
                        DeliveryMethod::ReliableOrdered,
                    )
                    .await;
            }

            let timeout = gloo_timers::future::TimeoutFuture::new(500);
            futures::select! {
                _ = futures::FutureExt::fuse(rx_peer) => {},
                _ = futures::FutureExt::fuse(timeout) => {
                    tracing::debug!("PeerResolver: Discovery timeout for {}", cid);
                }
            }
            known_peers = self.registry.get_peers(cid);
        }

        if !known_peers.is_empty() {
            use rand::seq::SliceRandom;
            let target = {
                let mut rng = rand::thread_rng();
                known_peers.choose(&mut rng).cloned()
            };

            if let Some(target) = target {
                tracing::debug!("PeerResolver: targeted WANT for {} to {}", cid, target.0);
                let mut want_msg = vec![MSG_WANT];
                want_msg.extend_from_slice(cid.as_bytes());
                // We don't track which session's transport actually knows
                // `target`, so fan the unicast WANT out too: transports
                // without a route to it just return an error, harmlessly.
                for transport in crate::app::all_session_transports() {
                    let _ = transport
                        .send(
                            &target,
                            bytes::Bytes::from(want_msg.clone()),
                            DeliveryMethod::ReliableOrdered,
                        )
                        .await;
                }
            }
        } else {
            tracing::debug!(
                "PeerResolver: no peers discovered, broadcasting WANT for {}",
                cid
            );
            let mut want_msg = vec![MSG_WANT];
            want_msg.extend_from_slice(cid.as_bytes());
            for transport in crate::app::all_session_transports() {
                let _ = transport
                    .broadcast(
                        bytes::Bytes::from(want_msg.clone()),
                        DeliveryMethod::ReliableOrdered,
                    )
                    .await;
            }
        }

        let timeout = gloo_timers::future::TimeoutFuture::new(self.timeout_ms);
        futures::select! {
            result = futures::FutureExt::fuse(rx_data) => result.ok(),
            _ = futures::FutureExt::fuse(timeout) => {
                self.registry.cancel(cid);
                tracing::debug!("PeerResolver: failed to receive data for CID {}", cid);
                None
            }
        }
    }
}
