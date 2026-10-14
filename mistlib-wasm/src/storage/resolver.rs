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

        let transports = crate::app::all_session_transports();
        if known_peers.is_empty() {
            use futures::{FutureExt, StreamExt};
            let fallback = async {
                let mut discovered = self.registry.register_peer_notifier(cid).fuse();
                let mut requested: std::collections::HashSet<_> =
                    self.registry.get_peers(cid).into_iter().collect();
                let recipients = transports
                    .iter()
                    .flat_map(|t| t.get_connected_nodes())
                    .chain(requested.iter().cloned())
                    .collect();
                let mut responses = futures::stream::FuturesUnordered::new();
                responses.push(self.registry.register_for_peers(cid, recipients));
                let mut want = vec![MSG_WANT];
                want.extend_from_slice(cid.as_bytes());
                let want = bytes::Bytes::from(want);
                for transport in &transports {
                    let _ = transport
                        .broadcast(want.clone(), DeliveryMethod::ReliableOrdered)
                        .await;
                }
                loop {
                    for peer in self.registry.get_peers(cid) {
                        if requested.insert(peer.clone()) {
                            responses.push(self.registry.register_from(cid, peer.clone()));
                            for transport in &transports {
                                let _ = transport
                                    .send(&peer, want.clone(), DeliveryMethod::ReliableOrdered)
                                    .await;
                            }
                        }
                    }
                    if responses.is_empty() {
                        let _ = (&mut discovered).await;
                        discovered = self.registry.register_peer_notifier(cid).fuse();
                        continue;
                    }
                    futures::select! {
                        result = responses.next().fuse() => {
                            if let Some(Ok(data)) = result { return data; }
                        },
                        _ = discovered => {
                            discovered = self.registry.register_peer_notifier(cid).fuse();
                        }
                    }
                }
            }
            .fuse();
            let timeout = gloo_timers::future::TimeoutFuture::new(self.timeout_ms).fuse();
            let result = {
                futures::pin_mut!(fallback, timeout);
                futures::select! {
                    data = fallback => Some(data),
                    _ = timeout => None,
                }
            };
            self.registry.cancel_closed();
            return result;
        }

        use rand::seq::SliceRandom;
        let target = known_peers
            .choose(&mut rand::thread_rng())
            .cloned()
            .expect("known peers is nonempty");
        let rx_data = self.registry.register_from(cid, target.clone());
        let mut want = vec![MSG_WANT];
        want.extend_from_slice(cid.as_bytes());
        for transport in transports {
            let _ = transport
                .send(
                    &target,
                    bytes::Bytes::from(want.clone()),
                    DeliveryMethod::ReliableOrdered,
                )
                .await;
        }

        let timeout = gloo_timers::future::TimeoutFuture::new(self.timeout_ms);
        let result = futures::select! {
            result = futures::FutureExt::fuse(rx_data) => result.ok(),
            _ = futures::FutureExt::fuse(timeout) => {
                tracing::debug!("PeerResolver: failed to receive data for CID {}", cid);
                None
            }
        };
        self.registry.cancel_closed();
        result
    }
}
