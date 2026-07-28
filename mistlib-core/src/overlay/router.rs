use self::dedupe::{OverlaySeenCache, OVERLAY_SEEN_MAX_ENTRIES, OVERLAY_SEEN_TTL};
use self::reorder::ReorderBuffer;
use crate::config::Config;
use crate::overlay::node_store::NodeStore;
use crate::overlay::routing_table::RoutingTable;
use crate::overlay::TopologyStrategy;
use crate::signaling::MessageContent;
use crate::types::{ConnectionState, NodeId};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

mod dedupe;
mod envelope;
mod reorder;
mod send;
mod strategies;

pub use envelope::HandleEnvelopeResult;

/// Maximum number of per-destination sequence counters kept before the least
/// recently used one is evicted. An evicted destination restarts at seq 1;
/// the receiver's reorder buffer treats seq 1 as a sender re-baseline.
const SEQ_COUNTER_MAX_DESTINATIONS: usize = 1024;

struct SeqCounter {
    value: u64,
    last_used: web_time::Instant,
}

pub struct OverlayRouter {
    pub node_store: Arc<Mutex<NodeStore>>,
    pub routing_table: Arc<Mutex<RoutingTable>>,
    pub strategies: Vec<Arc<dyn TopologyStrategy>>,
    pub local_node_id: NodeId,
    pub hop_count: u32,
    seen_envelopes: Mutex<OverlaySeenCache>,
    /// Per-destination monotonic sequence counters (sender side).
    seq_counters: Mutex<HashMap<NodeId, SeqCounter>>,
    /// Per-source reorder buffer (receiver side).
    reorder_buffer: Mutex<ReorderBuffer>,
}

impl OverlayRouter {
    pub fn new(config: &Config, node_store: Arc<Mutex<NodeStore>>, local_node_id: NodeId) -> Self {
        let routing_table = Arc::new(Mutex::new(RoutingTable::new()));

        Self {
            node_store,
            routing_table,
            strategies: Vec::new(),
            local_node_id,
            hop_count: config.limits.hop_count,
            seen_envelopes: Mutex::new(OverlaySeenCache::new(
                OVERLAY_SEEN_TTL,
                OVERLAY_SEEN_MAX_ENTRIES,
            )),
            seq_counters: Mutex::new(HashMap::new()),
            reorder_buffer: Mutex::new(ReorderBuffer::default()),
        }
    }

    /// Returns the next per-destination sequence number (monotonic, starting at 1).
    /// Bounded: the least recently used counter is evicted past the cap, so a
    /// long-running node under destination churn cannot grow this map forever.
    pub(crate) fn next_seq(&self, to: &NodeId) -> u64 {
        let now = web_time::Instant::now();
        let mut counters = self
            .seq_counters
            .lock()
            .expect("seq_counters lock poisoned");
        if counters.len() >= SEQ_COUNTER_MAX_DESTINATIONS && !counters.contains_key(to) {
            if let Some(oldest) = counters
                .iter()
                .min_by_key(|(_, c)| c.last_used)
                .map(|(id, _)| id.clone())
            {
                counters.remove(&oldest);
            }
        }
        let counter = counters.entry(to.clone()).or_insert(SeqCounter {
            value: 0,
            last_used: now,
        });
        counter.value += 1;
        counter.last_used = now;
        counter.value
    }

    /// Feeds a delivered message through the per-source reorder buffer, returning
    /// the messages now deliverable in order. `seq == 0` bypasses buffering.
    pub fn reorder_inbound(
        &self,
        from: &NodeId,
        seq: u64,
        content: MessageContent,
    ) -> Vec<MessageContent> {
        self.reorder_buffer
            .lock()
            .expect("reorder_buffer lock poisoned")
            .accept(from, seq, content)
    }

    /// Flushes any per-source reorder gaps that have been open past the gap
    /// timeout without new traffic to trigger `reorder_inbound`'s lazy flush
    /// (e.g. the sender went idle after a relay/direct route switch).
    /// Intended to be polled from an engine's periodic background tick so a
    /// stalled gap cannot wait forever for a message that will never arrive.
    /// Returns `(source, messages)` pairs, messages in seq order.
    pub fn flush_expired_inbound(&self) -> Vec<(NodeId, Vec<MessageContent>)> {
        self.reorder_buffer
            .lock()
            .expect("reorder_buffer lock poisoned")
            .flush_expired(web_time::Instant::now())
    }

    /// Test-only: replaces the reorder buffer with one using the given gap
    /// timeout. Production code always uses `REORDER_GAP_TIMEOUT` (8s, see
    /// reorder.rs); tests that need to exercise the gap-timeout flush path
    /// use this to inject a short timeout instead of sleeping for 8+ seconds.
    #[cfg(test)]
    pub(crate) fn set_reorder_gap_timeout_for_test(&self, gap_timeout: web_time::Duration) {
        *self
            .reorder_buffer
            .lock()
            .expect("reorder_buffer lock poisoned") = ReorderBuffer::new(
            self::reorder::REORDER_MAX_PER_SOURCE,
            self::reorder::REORDER_MAX_SOURCES,
            gap_timeout,
        );
    }

    /// Synchronises the routing table's direct connected set with a transport snapshot.
    pub fn sync_connection_states(
        &self,
        connected_node_states: &[(NodeId, ConnectionState)],
    ) -> HashSet<NodeId> {
        let connected = connected_node_states
            .iter()
            .filter(|(_, state)| *state == ConnectionState::Connected)
            .map(|(id, _)| id.clone())
            .collect::<HashSet<_>>();
        self.sync_connected_nodes(&connected);
        connected
    }

    pub fn sync_connected_nodes(&self, connected: &HashSet<NodeId>) {
        let mut rt = self
            .routing_table
            .lock()
            .expect("routing_table lock poisoned");
        let previous = rt.connected_nodes.clone();
        for id in connected {
            rt.on_connected(id.clone());
        }
        for id in previous {
            if !connected.contains(&id) {
                rt.on_disconnected(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> OverlayRouter {
        OverlayRouter::new(
            &Config::new_default(),
            Arc::new(Mutex::new(NodeStore::new())),
            NodeId("local".to_string()),
        )
    }

    #[test]
    fn sync_connection_states_keeps_only_connected_nodes() {
        let router = router();
        let connected = router.sync_connection_states(&[
            (NodeId("connected".to_string()), ConnectionState::Connected),
            (
                NodeId("connecting".to_string()),
                ConnectionState::Connecting,
            ),
            (
                NodeId("disconnected".to_string()),
                ConnectionState::Disconnected,
            ),
        ]);

        assert!(connected.contains(&NodeId("connected".to_string())));
        assert!(!connected.contains(&NodeId("connecting".to_string())));

        let rt = router.routing_table.lock().unwrap();
        assert!(rt
            .connected_nodes
            .contains(&NodeId("connected".to_string())));
        assert!(!rt
            .connected_nodes
            .contains(&NodeId("connecting".to_string())));
    }

    #[test]
    fn sync_connection_states_removes_routes_via_disconnected_nodes() {
        let router = router();
        let relay = NodeId("relay".to_string());
        let target = NodeId("target".to_string());
        router.sync_connection_states(&[(relay.clone(), ConnectionState::Connected)]);
        router
            .routing_table
            .lock()
            .unwrap()
            .add_route(target.clone(), relay.clone());

        router.sync_connection_states(&[]);

        assert_eq!(
            router.routing_table.lock().unwrap().get_next_hop(&target),
            None
        );
    }

    fn envelope(
        from: &str,
        to: NodeId,
        msg_id: u64,
        hop_count: u32,
    ) -> crate::overlay::OverlayEnvelope {
        crate::overlay::OverlayEnvelope {
            from: NodeId(from.to_string()),
            to,
            msg_id,
            seq: 0,
            hop_count,
            content: crate::signaling::MessageContent::Raw(bytes::Bytes::from_static(b"payload")),
        }
    }

    #[test]
    fn handle_envelope_drops_duplicate_msg_id() {
        let router = router();
        let peer = NodeId("peer-a".to_string());
        let other = NodeId("peer-b".to_string());
        router.sync_connection_states(&[
            (peer.clone(), ConnectionState::Connected),
            (other.clone(), ConnectionState::Connected),
        ]);
        let envelope = envelope("peer-a", NodeId::broadcast(), 42, 2);

        let first = router.handle_envelope(envelope.clone(), peer.clone());
        assert!(first.should_deliver);
        assert_eq!(first.actions.len(), 1);

        let second = router.handle_envelope(envelope, other);
        assert!(!second.should_deliver);
        assert!(second.actions.is_empty());
    }

    #[test]
    fn handle_envelope_zero_msg_id_is_not_deduped() {
        let router = router();
        let peer = NodeId("peer-a".to_string());
        let other = NodeId("peer-b".to_string());
        router.sync_connection_states(&[
            (peer.clone(), ConnectionState::Connected),
            (other.clone(), ConnectionState::Connected),
        ]);
        let envelope = envelope("peer-a", NodeId::broadcast(), 0, 2);

        let first = router.handle_envelope(envelope.clone(), peer.clone());
        let second = router.handle_envelope(envelope, peer);

        assert!(first.should_deliver);
        assert!(second.should_deliver);
        assert_eq!(first.actions.len(), 1);
        assert_eq!(second.actions.len(), 1);
    }

    #[test]
    fn handle_envelope_drops_local_echo() {
        let router = router();
        let envelope = envelope("local", NodeId::broadcast(), 99, 2);

        let result = router.handle_envelope(envelope, NodeId("peer-a".to_string()));

        assert!(!result.should_deliver);
        assert!(result.actions.is_empty());
    }

    #[test]
    fn broadcast_direct_and_relayed_duplicate_delivers_once() {
        let router_b = router();
        let a = NodeId("node-a".to_string());
        let c = NodeId("node-c".to_string());
        router_b.sync_connection_states(&[
            (a.clone(), ConnectionState::Connected),
            (c.clone(), ConnectionState::Connected),
        ]);
        let direct = envelope("node-a", NodeId::broadcast(), 7, 2);
        let mut relayed = direct.clone();
        relayed.hop_count = 1;

        let direct_result = router_b.handle_envelope(direct, a.clone());
        let relayed_result = router_b.handle_envelope(relayed, c);

        assert!(direct_result.should_deliver);
        assert_eq!(
            direct_result.actions.len(),
            1,
            "B should forward direct A broadcast to C"
        );
        assert!(!relayed_result.should_deliver);
        assert!(relayed_result.actions.is_empty());
    }

    #[test]
    fn outgoing_envelopes_use_nonzero_msg_id_and_are_remembered() {
        let router = router();
        let action = router.wrap_data(
            &NodeId::broadcast(),
            bytes::Bytes::from_static(b"payload"),
            crate::types::DeliveryMethod::ReliableOrdered,
        );
        let crate::action::OverlayAction::SendMessage { data, .. } = action else {
            panic!("wrap_data should produce SendMessage");
        };
        let env: crate::overlay::OverlayEnvelope =
            crate::overlay::wire::deserialize(&data).unwrap();

        assert_ne!(env.msg_id, 0);
        let echo = router.handle_envelope(env, NodeId("peer-a".to_string()));
        assert!(!echo.should_deliver);
        assert!(echo.actions.is_empty());
    }

    fn seq_of(action: crate::action::OverlayAction) -> u64 {
        let crate::action::OverlayAction::SendMessage { data, .. } = action else {
            panic!("wrap_data should produce SendMessage");
        };
        let env: crate::overlay::OverlayEnvelope =
            crate::overlay::wire::deserialize(&data).unwrap();
        env.seq
    }

    #[test]
    fn reliable_unicast_gets_monotonic_per_destination_seq() {
        let router = router();
        let dest = NodeId("dest".to_string());
        let payload = bytes::Bytes::from_static(b"payload");
        let m = crate::types::DeliveryMethod::ReliableOrdered;

        assert_eq!(seq_of(router.wrap_data(&dest, payload.clone(), m)), 1);
        assert_eq!(seq_of(router.wrap_data(&dest, payload.clone(), m)), 2);
        // Independent counter per destination.
        let other = NodeId("other".to_string());
        assert_eq!(seq_of(router.wrap_data(&other, payload, m)), 1);
    }

    #[test]
    fn broadcast_and_unreliable_carry_no_seq() {
        let router = router();
        let dest = NodeId("dest".to_string());
        let payload = bytes::Bytes::from_static(b"payload");

        // Broadcast destination: never sequenced.
        assert_eq!(
            seq_of(router.wrap_data(
                &NodeId::broadcast(),
                payload.clone(),
                crate::types::DeliveryMethod::ReliableOrdered
            )),
            0
        );
        // Non-reliable methods: never sequenced.
        assert_eq!(
            seq_of(router.wrap_data(&dest, payload, crate::types::DeliveryMethod::Unreliable)),
            0
        );
    }

    #[test]
    fn reorder_inbound_orders_per_source_and_bypasses_zero_seq() {
        let router = router();
        let src = NodeId("src".to_string());
        let raw =
            |t: &[u8]| crate::signaling::MessageContent::Raw(bytes::Bytes::copy_from_slice(t));

        // seq 2 buffered, seq 1 unblocks both.
        assert!(router.reorder_inbound(&src, 2, raw(b"m2")).is_empty());
        assert_eq!(router.reorder_inbound(&src, 1, raw(b"m1")).len(), 2);
        // seq 0 bypasses.
        assert_eq!(router.reorder_inbound(&src, 0, raw(b"ctrl")).len(), 1);
    }

    // --- Characterization tests -------------------------------------------
    //
    // These record the CURRENT behavior of the ReliableOrdered gap-timeout
    // path end-to-end, through the same public API mistlib-native's
    // engine::network.rs receive path calls (`reorder_inbound`) and that
    // wrap_data's sender-side seq assignment feeds. This is NOT a
    // specification of desired behavior: a gap that outlives
    // REORDER_GAP_TIMEOUT is silently skipped (no error surfaced to the
    // caller, see `SourceState::flush` in reorder.rs) and the eventually-late
    // message is then silently dropped (`seq < state.next_expected`). For a
    // byte-stream consumer layered on top (e.g. a tunneled TCP/SSH session)
    // this corrupts the stream with a permanent hole. Likewise, an
    // application-level retry that re-wraps the same payload gets a fresh
    // seq/msg_id and can be delivered twice. See REORDER_RELIABILITY_NOTES.md
    // for the options considered to change this. If this behavior is ever
    // intentionally changed, these tests are expected to be updated or
    // deleted, not treated as a regression.

    fn char_chunk(tag: u8) -> Vec<u8> {
        // A distinct, recognizable 8-byte slice of a hypothetical byte stream.
        vec![tag; 8]
    }

    fn char_raw(payload: &[u8]) -> crate::signaling::MessageContent {
        crate::signaling::MessageContent::Raw(bytes::Bytes::copy_from_slice(payload))
    }

    fn char_stream_bytes(delivered: &[crate::signaling::MessageContent]) -> Vec<u8> {
        // Reassembles the byte stream exactly like a naive consumer would:
        // write each payload out in arrival order, nothing else.
        delivered
            .iter()
            .flat_map(|c| match c {
                crate::signaling::MessageContent::Raw(b) => b.to_vec(),
                _ => panic!("unexpected content"),
            })
            .collect()
    }

    #[test]
    fn characterization_delayed_chunk_makes_reassembled_stream_corrupt_and_is_dropped_forever() {
        let router = router();
        // Inject a short gap timeout so this test doesn't have to sleep past
        // the production REORDER_GAP_TIMEOUT (8s, reorder.rs). The behavior
        // exercised is identical -- only the wall-clock threshold differs.
        router.set_reorder_gap_timeout_for_test(web_time::Duration::from_millis(50));
        let src = NodeId("peer-sender".to_string());

        // The sender-side byte stream: 6 chunks, seq 1..=6.
        let original: Vec<u8> = (1..=6u8).flat_map(char_chunk).collect();
        let mut delivered: Vec<crate::signaling::MessageContent> = Vec::new();

        // seq 1 arrives, delivers immediately.
        delivered.extend(router.reorder_inbound(&src, 1, char_raw(&char_chunk(1))));

        // seq 2 is delayed in flight (e.g. a route switch). seq 3..=5 arrive
        // normally and are buffered waiting for the gap to fill.
        for s in 3..=5u8 {
            delivered.extend(router.reorder_inbound(&src, s as u64, char_raw(&char_chunk(s))));
        }
        assert_eq!(
            char_stream_bytes(&delivered),
            char_chunk(1),
            "3..5 are held back while the gap at seq 2 is fresh"
        );

        // The gap stays open past the injected gap timeout (50ms above). The
        // public API has no clock-injection hook (accept_at is private to
        // the reorder module), so this uses a real sleep; the timeout was
        // shortened via `set_reorder_gap_timeout_for_test` specifically so
        // this sleep can stay short instead of waiting out the production
        // 8s REORDER_GAP_TIMEOUT.
        std::thread::sleep(std::time::Duration::from_millis(80));

        // Next arrival (seq 6) lazily flushes the gap: 3,4,5,6 are delivered
        // WITHOUT seq 2.
        delivered.extend(router.reorder_inbound(&src, 6, char_raw(&char_chunk(6))));

        let received = char_stream_bytes(&delivered);
        let expected_so_far: Vec<u8> = [1u8, 3, 4, 5, 6]
            .iter()
            .flat_map(|&t| char_chunk(t))
            .collect();
        assert_eq!(
            received, expected_so_far,
            "stream is delivered with an 8-byte hole where chunk 2 belongs"
        );
        assert_ne!(
            received, original,
            "the reassembled byte stream no longer matches what the sender wrote"
        );

        // Now the delayed chunk 2 finally arrives. It is dropped
        // unconditionally: seq < next_expected.
        let late = router.reorder_inbound(&src, 2, char_raw(&char_chunk(2)));
        assert!(
            late.is_empty(),
            "the late chunk is dropped forever -- the 8-byte hole is permanent"
        );
    }

    #[test]
    fn characterization_app_level_retry_gets_fresh_seq_and_msg_id_so_receiver_delivers_it_twice() {
        // --- sender side: same payload wrapped twice (send + retry) ---
        let sender = router();
        let dest = NodeId("local-receiver".to_string());
        let payload = char_chunk(9);

        let mut envelopes = Vec::new();
        for _ in 0..2 {
            let action = sender.wrap_data(
                &dest,
                bytes::Bytes::copy_from_slice(&payload),
                crate::types::DeliveryMethod::ReliableOrdered,
            );
            let crate::action::OverlayAction::SendMessage { data, .. } = action else {
                panic!("wrap_data must produce SendMessage");
            };
            let env: crate::overlay::OverlayEnvelope =
                crate::overlay::wire::deserialize(&data).expect("decode envelope");
            envelopes.push(env);
        }

        assert_ne!(
            envelopes[0].seq, envelopes[1].seq,
            "retry gets a fresh seq (next_seq increments per call)"
        );
        assert_ne!(
            envelopes[0].msg_id, envelopes[1].msg_id,
            "retry gets a fresh msg_id, so (from, msg_id) dedup cannot catch it"
        );

        // --- receiver side: both copies arrive; both are delivered ---
        let receiver = router();
        let mut delivered = Vec::new();
        for env in &envelopes {
            delivered.extend(receiver.reorder_inbound(&env.from, env.seq, env.content.clone()));
        }
        assert_eq!(
            char_stream_bytes(&delivered),
            [char_chunk(9), char_chunk(9)].concat(),
            "the same 8 bytes are written into the stream twice"
        );
    }
}
