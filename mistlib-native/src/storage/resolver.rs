use async_trait::async_trait;
pub use mistlib_core::storage::protocol::{
    build_have_chunk_message, build_have_message, build_have_status_message, build_query_message,
    parse_have_chunk_message, parse_have_message, parse_have_status_message, parse_query_message,
    parse_want_message, WantRegistry, HAVE_CHUNK_SIZE, MSG_HAVE, MSG_HAVE_CHUNK, MSG_HAVE_STATUS,
    MSG_QUERY, MSG_WANT,
};
use mistlib_core::storage::PeerResolver;
use mistlib_core::transport::Transport;
use mistlib_core::types::DeliveryMethod;
use std::sync::Arc;

/// Supplies the set of transports a WANT/QUERY broadcast should fan out
/// across, snapshotted fresh on every call (SPEC-15 rule 8: in production
/// this is one transport per active room session, since a block's peers may
/// only be reachable through one particular room). Exists so
/// `NativePeerResolver` doesn't need a direct dependency on the engine's
/// session registry -- and so tests can supply a fixed transport list.
#[async_trait]
pub trait TransportSource: Send + Sync {
    async fn transports(&self) -> Vec<Arc<dyn Transport>>;
}

/// A fixed set of transports, for tests (and any embedder without a live
/// session registry to query).
pub struct FixedTransportSource(pub Vec<Arc<dyn Transport>>);

#[async_trait]
impl TransportSource for FixedTransportSource {
    async fn transports(&self) -> Vec<Arc<dyn Transport>> {
        self.0.clone()
    }
}

pub struct NativePeerResolver {
    transports: Arc<dyn TransportSource>,
    registry: WantRegistry,
    timeout_ms: u64,
    /// Round-robin cursor so successive chunk requests are spread across the
    /// known peers instead of repeatedly hitting one (client/server style).
    next_peer: std::sync::atomic::AtomicUsize,
}

impl NativePeerResolver {
    pub fn new(
        transports: Arc<dyn TransportSource>,
        registry: WantRegistry,
        timeout_ms: u64,
    ) -> Self {
        Self {
            transports,
            registry,
            timeout_ms,
            next_peer: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Broadcasts `data` on every currently-active transport (fire-and-forget:
    /// a room this WANT/QUERY isn't relevant to simply has no matching peer).
    async fn broadcast_all(&self, data: bytes::Bytes) {
        for transport in self.transports.transports().await {
            let _ = transport
                .broadcast(data.clone(), DeliveryMethod::ReliableOrdered)
                .await;
        }
    }

    /// Sends `data` to `target` on every currently-active transport. We don't
    /// track which room a peer was discovered through, so -- like
    /// `broadcast_all` -- this fans out and lets `Transport::send` fail
    /// harmlessly on any transport that doesn't have `target` connected.
    async fn send_all(&self, target: &mistlib_core::types::NodeId, data: bytes::Bytes) {
        for transport in self.transports.transports().await {
            let _ = transport
                .send(target, data.clone(), DeliveryMethod::ReliableOrdered)
                .await;
        }
    }
}

#[async_trait]
impl PeerResolver for NativePeerResolver {
    async fn resolve_block(&self, cid: &str) -> Option<Vec<u8>> {
        let mut known_peers = self.registry.get_peers(cid);
        if known_peers.is_empty() {
            tracing::debug!("PeerResolver: Discovery phase for {}", cid);
            let rx_peer = self.registry.register_peer_notifier(cid);

            let query_msg = build_query_message(cid);
            self.broadcast_all(bytes::Bytes::from(query_msg)).await;

            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), rx_peer).await;
            known_peers = self.registry.get_peers(cid);
        }

        let per_attempt = std::time::Duration::from_millis(self.timeout_ms);

        // Try each known peer in round-robin order, failing over to the next one
        // if the chosen peer does not deliver the block within the deadline. The
        // starting offset is shared across calls so concurrent chunk requests
        // spread their load across the swarm instead of hammering one node.
        if !known_peers.is_empty() {
            let start = self
                .next_peer
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            for offset in 0..known_peers.len() {
                let target = &known_peers[(start + offset) % known_peers.len()];
                let rx_data = self.registry.register(cid);

                tracing::debug!("PeerResolver: targeted WANT for {} to {}", cid, target.0);
                let mut want_msg = vec![MSG_WANT];
                want_msg.extend_from_slice(cid.as_bytes());
                self.send_all(target, bytes::Bytes::from(want_msg)).await;

                if let Ok(Ok(data)) = tokio::time::timeout(per_attempt, rx_data).await {
                    return Some(data);
                }
                tracing::debug!(
                    "PeerResolver: peer {} did not deliver {}, failing over",
                    target.0,
                    cid
                );
            }
        }

        // No known peers, or every known peer failed: broadcast a last-resort WANT.
        tracing::debug!("PeerResolver: broadcasting fallback WANT for {}", cid);
        let rx_data = self.registry.register(cid);
        let mut want_msg = vec![MSG_WANT];
        want_msg.extend_from_slice(cid.as_bytes());
        self.broadcast_all(bytes::Bytes::from(want_msg)).await;

        match tokio::time::timeout(per_attempt, rx_data).await {
            Ok(Ok(data)) => Some(data),
            _ => {
                self.registry.cancel(cid);
                tracing::debug!("PeerResolver: failed to receive data for CID {}", cid);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn have_chunk_message_roundtrips() {
        let msg = build_have_chunk_message("cid-1", 1, 3, b"payload");
        let parsed = parse_have_chunk_message(&msg).expect("chunk message should parse");
        assert_eq!(parsed.0, "cid-1");
        assert_eq!(parsed.1, 1);
        assert_eq!(parsed.2, 3);
        assert_eq!(parsed.3, b"payload");
    }

    #[tokio::test]
    async fn registry_reassembles_have_chunks() {
        let registry = WantRegistry::new();
        let rx = registry.register("cid-1");

        registry.fulfill_chunk("cid-1", 1, 3, b"bb".to_vec());
        registry.fulfill_chunk("cid-1", 0, 3, b"aa".to_vec());
        registry.fulfill_chunk("cid-1", 2, 3, b"cc".to_vec());

        let data = rx.await.expect("assembled payload should be delivered");
        assert_eq!(data, b"aabbcc");
    }

    type RecordedSends = Arc<Mutex<Vec<(mistlib_core::types::NodeId, Vec<u8>)>>>;

    /// Transport stub that records every targeted `send` so a test can inspect
    /// which peers received WANT requests.
    struct RecordingTransport {
        sends: RecordedSends,
    }

    #[async_trait]
    impl Transport for RecordingTransport {
        async fn start(
            &self,
            _handler: Arc<dyn mistlib_core::transport::NetworkEventHandler>,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        async fn send(
            &self,
            node: &mistlib_core::types::NodeId,
            data: bytes::Bytes,
            _method: DeliveryMethod,
        ) -> mistlib_core::error::Result<()> {
            self.sends
                .lock()
                .unwrap()
                .push((node.clone(), data.to_vec()));
            Ok(())
        }
        async fn broadcast(
            &self,
            _data: bytes::Bytes,
            _method: DeliveryMethod,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        fn get_connection_state(
            &self,
            _node: &mistlib_core::types::NodeId,
        ) -> mistlib_core::types::ConnectionState {
            mistlib_core::types::ConnectionState::Connected
        }
        async fn connect(
            &self,
            _node: &mistlib_core::types::NodeId,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        async fn disconnect(
            &self,
            _node: &mistlib_core::types::NodeId,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        fn get_connected_nodes(&self) -> Vec<mistlib_core::types::NodeId> {
            Vec::new()
        }
    }

    #[tokio::test]
    async fn chunk_wants_are_distributed_across_multiple_peers() {
        use mistlib_core::storage::PeerResolver;
        use mistlib_core::types::NodeId;

        let sends = Arc::new(Mutex::new(Vec::new()));
        let transport = Arc::new(RecordingTransport {
            sends: sends.clone(),
        });
        let registry = WantRegistry::new();

        // Three peers all advertise the same set of chunks.
        let peers = [
            NodeId("peer-a".into()),
            NodeId("peer-b".into()),
            NodeId("peer-c".into()),
        ];
        let chunk_cids = [
            "chunk-0", "chunk-1", "chunk-2", "chunk-3", "chunk-4", "chunk-5",
        ];
        for cid in chunk_cids {
            for peer in &peers {
                registry.register_peer(cid, peer.clone());
            }
        }

        // Short timeout: we never fulfill, so each resolve sends its WANT then
        // times out. We only care about the recorded targets.
        let resolver = NativePeerResolver::new(
            Arc::new(FixedTransportSource(vec![transport])),
            registry,
            30,
        );

        // Resolve all chunks concurrently, mirroring the engine's parallel fan-out.
        let mut tasks = Vec::new();
        for cid in chunk_cids {
            let r = &resolver;
            tasks.push(async move { r.resolve_block(cid).await });
        }
        let _ = futures_util::future::join_all(tasks).await;

        // We never fulfill, so each chunk fails over through every known peer.
        // The load-distribution property lives in the *first* WANT each chunk
        // issues: round-robin must spread those across the swarm rather than
        // starting every request at the same node (client/server style).
        let first_target_per_cid: std::collections::BTreeMap<String, String> = {
            let mut map = std::collections::BTreeMap::new();
            for (node, data) in sends.lock().unwrap().iter() {
                if let Some(cid) = parse_want_message(data) {
                    map.entry(cid).or_insert_with(|| node.0.clone());
                }
            }
            map
        };

        assert_eq!(
            first_target_per_cid.len(),
            chunk_cids.len(),
            "every chunk issues a WANT"
        );

        let distinct_starts: std::collections::BTreeSet<_> =
            first_target_per_cid.values().cloned().collect();
        assert!(
            distinct_starts.len() > 1,
            "chunk requests all started at one peer (client/server style): {distinct_starts:?}"
        );
        // Round-robin over 3 peers for 6 chunks must start across all three.
        assert_eq!(
            distinct_starts.len(),
            peers.len(),
            "round-robin should spread initial WANTs across every known peer: {distinct_starts:?}"
        );
    }

    /// Transport that only the designated `good_peer` answers: a WANT sent to it
    /// is fulfilled through the registry, while WANTs to any other peer are
    /// silently dropped (simulating an unresponsive/dead peer).
    struct FailoverTransport {
        registry: WantRegistry,
        good_peer: mistlib_core::types::NodeId,
        block: Vec<u8>,
        sends: Arc<Mutex<Vec<mistlib_core::types::NodeId>>>,
    }

    #[async_trait]
    impl Transport for FailoverTransport {
        async fn start(
            &self,
            _handler: Arc<dyn mistlib_core::transport::NetworkEventHandler>,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        async fn send(
            &self,
            node: &mistlib_core::types::NodeId,
            data: bytes::Bytes,
            _method: DeliveryMethod,
        ) -> mistlib_core::error::Result<()> {
            self.sends.lock().unwrap().push(node.clone());
            if let Some(cid) = parse_want_message(&data) {
                if *node == self.good_peer {
                    self.registry.fulfill(&cid, self.block.clone());
                }
            }
            Ok(())
        }
        async fn broadcast(
            &self,
            _data: bytes::Bytes,
            _method: DeliveryMethod,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        fn get_connection_state(
            &self,
            _node: &mistlib_core::types::NodeId,
        ) -> mistlib_core::types::ConnectionState {
            mistlib_core::types::ConnectionState::Connected
        }
        async fn connect(
            &self,
            _node: &mistlib_core::types::NodeId,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        async fn disconnect(
            &self,
            _node: &mistlib_core::types::NodeId,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        fn get_connected_nodes(&self) -> Vec<mistlib_core::types::NodeId> {
            Vec::new()
        }
    }

    #[tokio::test]
    async fn unresponsive_peer_fails_over_to_a_live_peer() {
        use mistlib_core::storage::PeerResolver;
        use mistlib_core::types::NodeId;

        let registry = WantRegistry::new();
        let dead = NodeId("dead-peer".into());
        let good = NodeId("good-peer".into());
        // Order matters: round-robin starts at the dead peer, forcing a failover.
        registry.register_peer("cid-x", dead.clone());
        registry.register_peer("cid-x", good.clone());

        let sends = Arc::new(Mutex::new(Vec::new()));
        let transport = Arc::new(FailoverTransport {
            registry: registry.clone(),
            good_peer: good.clone(),
            block: b"payload-from-good-peer".to_vec(),
            sends: sends.clone(),
        });

        // Short per-attempt timeout so the dead peer's attempt fails quickly.
        let resolver = NativePeerResolver::new(
            Arc::new(FixedTransportSource(vec![transport])),
            registry,
            40,
        );
        let data = resolver.resolve_block("cid-x").await;

        assert_eq!(
            data.as_deref(),
            Some(b"payload-from-good-peer".as_slice()),
            "download should succeed by failing over to the live peer"
        );

        let targets = sends.lock().unwrap().clone();
        assert_eq!(
            targets,
            vec![dead, good],
            "resolver should try the dead peer first, then fail over to the live one"
        );
    }
}
