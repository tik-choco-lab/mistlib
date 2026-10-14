use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

use crate::types::NodeId;

pub const MSG_WANT: u8 = 0x01;
pub const MSG_HAVE: u8 = 0x02;
pub const MSG_QUERY: u8 = 0x03;
pub const MSG_HAVE_STATUS: u8 = 0x04;
pub const MSG_HAVE_CHUNK: u8 = 0x05;
pub const HAVE_CHUNK_SIZE: usize = 16 * 1024;

// Wire chunk sizes are negotiated; bound bytes independently of chunk count.
pub const MAX_WANT_BLOCK_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_WANT_ASSEMBLY_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_WANT_CHUNKS: u16 = 8192;
pub const MAX_WANT_ASSEMBLIES: usize = 256;
pub const MAX_PENDING_WANTS: usize = 1024;
const MAX_WANT_PEERS: usize = 1024;

type PeerCache = Arc<Mutex<HashMap<String, Vec<NodeId>>>>;
type PeerNotifiers = Arc<Mutex<HashMap<String, Vec<oneshot::Sender<()>>>>>;

struct PendingWant {
    peers: Vec<NodeId>,
    tx: oneshot::Sender<Vec<u8>>,
}

struct ChunkAssembly {
    total_chunks: u16,
    received_chunks: u16,
    bytes: usize,
    chunks: Vec<Option<Vec<u8>>>,
}

#[derive(Default)]
struct WantState {
    pending: HashMap<String, Vec<PendingWant>>,
    assemblies: HashMap<(String, NodeId), ChunkAssembly>,
}

impl WantState {
    fn accepts(&self, cid: &str, sender: &NodeId) -> bool {
        self.pending.get(cid).is_some_and(|wants| {
            wants
                .iter()
                .any(|want| !want.tx.is_closed() && want.peers.contains(sender))
        })
    }

    fn prune(&mut self) {
        self.pending.retain(|_, wants| {
            wants.retain(|want| !want.tx.is_closed());
            !wants.is_empty()
        });
        let pending = &self.pending;
        self.assemblies.retain(|(cid, sender), _| {
            pending
                .get(cid)
                .is_some_and(|wants| wants.iter().any(|want| want.peers.contains(sender)))
        });
    }

    fn fulfill(&mut self, cid: &str, sender: &NodeId, data: Vec<u8>) {
        if let Some(wants) = self.pending.remove(cid) {
            // Unverified bytes may only complete waiters that requested this sender.
            let (authorized, remaining): (Vec<_>, Vec<_>) = wants
                .into_iter()
                .partition(|want| want.peers.contains(sender));
            if !remaining.is_empty() {
                self.pending.insert(cid.to_string(), remaining);
            }
            self.prune();
            let mut wants = authorized.into_iter().peekable();
            while let Some(want) = wants.next() {
                if wants.peek().is_none() {
                    let _ = want.tx.send(data);
                    break;
                }
                let _ = want.tx.send(data.clone());
            }
        }
    }
}

#[derive(Clone)]
pub struct WantRegistry {
    state: Arc<Mutex<WantState>>,
    peer_cache: PeerCache,
    peer_notifiers: PeerNotifiers,
}

impl Default for WantRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WantRegistry {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(WantState::default())),
            peer_cache: Arc::new(Mutex::new(HashMap::new())),
            peer_notifiers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Local/in-process compatibility API. Network resolvers must register actual senders.
    pub fn register(&self, cid: &str) -> oneshot::Receiver<Vec<u8>> {
        self.register_from(cid, NodeId(String::new()))
    }

    pub fn register_from(&self, cid: &str, sender: NodeId) -> oneshot::Receiver<Vec<u8>> {
        self.register_for_peers(cid, vec![sender])
    }

    /// Register before sending WANT; for broadcast, pass the snapshotted recipients.
    /// An immediately closed receiver indicates admission limits were exceeded.
    pub fn register_for_peers(
        &self,
        cid: &str,
        mut peers: Vec<NodeId>,
    ) -> oneshot::Receiver<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        let mut state = self.state.lock().unwrap();
        state.prune();
        if cid.len() > u8::MAX as usize
            || peers.is_empty()
            || peers.len() > MAX_WANT_PEERS
            || peers.iter().any(|peer| peer.0.len() > 256)
            || state.pending.values().map(Vec::len).sum::<usize>() >= MAX_PENDING_WANTS
        {
            return rx;
        }
        peers.sort_by(|a, b| a.0.cmp(&b.0));
        peers.dedup();
        state
            .pending
            .entry(cid.to_string())
            .or_default()
            .push(PendingWant { peers, tx });
        rx
    }

    pub fn fulfill(&self, cid: &str, data: Vec<u8>) {
        self.fulfill_from(cid, &NodeId(String::new()), data);
    }

    pub fn fulfill_from(&self, cid: &str, sender: &NodeId, data: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        state.prune();
        if data.len() <= MAX_WANT_BLOCK_BYTES && state.accepts(cid, sender) {
            state.fulfill(cid, sender, data);
        }
    }

    pub fn fulfill_chunk(&self, cid: &str, chunk_index: u16, chunk_total: u16, data: Vec<u8>) {
        self.fulfill_chunk_from(cid, &NodeId(String::new()), chunk_index, chunk_total, data);
    }

    pub fn fulfill_chunk_from(
        &self,
        cid: &str,
        sender: &NodeId,
        chunk_index: u16,
        chunk_total: u16,
        data: Vec<u8>,
    ) {
        if chunk_total == 0
            || chunk_total > MAX_WANT_CHUNKS
            || chunk_index >= chunk_total
            || data.len() > MAX_WANT_BLOCK_BYTES
        {
            return;
        }
        // One lock makes admission, allocation, completion and cancellation atomic.
        let mut state = self.state.lock().unwrap();
        state.prune();
        if !state.accepts(cid, sender) {
            return;
        }
        let key = (cid.to_string(), sender.clone());
        if let Some(entry) = state.assemblies.get(&key) {
            // Never reset an assembly in response to contradictory metadata.
            if entry.total_chunks != chunk_total || entry.chunks[chunk_index as usize].is_some() {
                return;
            }
            if entry.bytes + data.len() > MAX_WANT_BLOCK_BYTES {
                return;
            }
        } else if state.assemblies.len() >= MAX_WANT_ASSEMBLIES {
            return;
        }
        if state
            .assemblies
            .values()
            .map(|entry| entry.bytes)
            .sum::<usize>()
            + data.len()
            > MAX_WANT_ASSEMBLY_BYTES
        {
            return;
        }
        let entry = state
            .assemblies
            .entry(key.clone())
            .or_insert_with(|| ChunkAssembly {
                total_chunks: chunk_total,
                received_chunks: 0,
                bytes: 0,
                chunks: (0..chunk_total).map(|_| None).collect(),
            });
        entry.bytes += data.len();
        entry.chunks[chunk_index as usize] = Some(data);
        entry.received_chunks += 1;
        if entry.received_chunks == entry.total_chunks {
            let mut full = Vec::with_capacity(entry.bytes);
            for part in &mut entry.chunks {
                full.extend_from_slice(&part.take().unwrap());
            }
            state.assemblies.remove(&key);
            state.fulfill(cid, sender, full);
        }
    }

    /// Remove only abandoned waiters, preserving other concurrent requests.
    pub fn cancel_closed(&self) {
        self.state.lock().unwrap().prune();
        self.peer_notifiers.lock().unwrap().retain(|_, senders| {
            senders.retain(|tx| !tx.is_closed());
            !senders.is_empty()
        });
    }

    /// Explicitly cancel all requests for a CID. A single resolver timeout must not use this.
    pub fn cancel(&self, cid: &str) {
        let mut state = self.state.lock().unwrap();
        state.pending.remove(cid);
        state.assemblies.retain(|(key, _), _| key != cid);
        self.peer_notifiers.lock().unwrap().remove(cid);
    }

    pub fn register_peer_notifier(&self, cid: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.cancel_closed();
        self.peer_notifiers
            .lock()
            .unwrap()
            .entry(cid.to_string())
            .or_default()
            .push(tx);
        rx
    }

    pub fn register_peer(&self, cid: &str, peer_id: NodeId) {
        {
            let mut cache = self.peer_cache.lock().unwrap();
            let peers = cache.entry(cid.to_string()).or_default();
            if !peers.contains(&peer_id) {
                peers.push(peer_id);
            }
        }
        if let Some(notifiers) = self.peer_notifiers.lock().unwrap().remove(cid) {
            for tx in notifiers {
                let _ = tx.send(());
            }
        }
    }

    pub fn get_peers(&self, cid: &str) -> Vec<NodeId> {
        self.peer_cache
            .lock()
            .unwrap()
            .get(cid)
            .cloned()
            .unwrap_or_default()
    }
}

pub fn parse_have_message(raw: &[u8]) -> Option<(String, Vec<u8>)> {
    if raw.len() < 2 || raw[0] != MSG_HAVE {
        return None;
    }
    let cid_len = raw[1] as usize;
    if raw.len() < 2 + cid_len {
        return None;
    }
    let cid = std::str::from_utf8(&raw[2..2 + cid_len]).ok()?.to_string();
    Some((cid, raw[2 + cid_len..].to_vec()))
}

pub fn build_have_message(cid: &str, data: &[u8]) -> Vec<u8> {
    let cb = cid.as_bytes();
    let mut msg = Vec::with_capacity(2 + cb.len() + data.len());
    msg.push(MSG_HAVE);
    msg.push(cb.len() as u8);
    msg.extend_from_slice(cb);
    msg.extend_from_slice(data);
    msg
}

pub fn build_have_chunk_message(
    cid: &str,
    chunk_index: u16,
    chunk_total: u16,
    data: &[u8],
) -> Vec<u8> {
    let cb = cid.as_bytes();
    let mut msg = Vec::with_capacity(6 + cb.len() + data.len());
    msg.push(MSG_HAVE_CHUNK);
    msg.push(cb.len() as u8);
    msg.extend_from_slice(&chunk_index.to_be_bytes());
    msg.extend_from_slice(&chunk_total.to_be_bytes());
    msg.extend_from_slice(cb);
    msg.extend_from_slice(data);
    msg
}

pub fn parse_have_chunk_message(raw: &[u8]) -> Option<(String, u16, u16, Vec<u8>)> {
    if raw.len() < 6 || raw[0] != MSG_HAVE_CHUNK {
        return None;
    }

    let cid_len = raw[1] as usize;
    let header_end = 6 + cid_len;
    if raw.len() < header_end {
        return None;
    }

    let chunk_index = u16::from_be_bytes([raw[2], raw[3]]);
    let chunk_total = u16::from_be_bytes([raw[4], raw[5]]);
    if chunk_total == 0 || chunk_index >= chunk_total {
        return None;
    }

    let cid = std::str::from_utf8(&raw[6..header_end]).ok()?.to_string();
    let payload = raw[header_end..].to_vec();
    Some((cid, chunk_index, chunk_total, payload))
}

pub fn parse_want_message(raw: &[u8]) -> Option<String> {
    if raw.is_empty() || raw[0] != MSG_WANT {
        return None;
    }
    std::str::from_utf8(&raw[1..]).ok().map(|s| s.to_string())
}

pub fn build_query_message(cid: &str) -> Vec<u8> {
    let mut msg = vec![MSG_QUERY];
    msg.extend_from_slice(cid.as_bytes());
    msg
}

pub fn parse_query_message(raw: &[u8]) -> Option<String> {
    if raw.is_empty() || raw[0] != MSG_QUERY {
        return None;
    }
    std::str::from_utf8(&raw[1..]).ok().map(|s| s.to_string())
}

pub fn build_have_status_message(cid: &str) -> Vec<u8> {
    let mut msg = vec![MSG_HAVE_STATUS];
    msg.extend_from_slice(cid.as_bytes());
    msg
}

pub fn parse_have_status_message(raw: &[u8]) -> Option<String> {
    if raw.is_empty() || raw[0] != MSG_HAVE_STATUS {
        return None;
    }
    std::str::from_utf8(&raw[1..]).ok().map(|s| s.to_string())
}

/// Picks the wire chunk size for one `cid`'s `HAVE_CHUNK` sends from the
/// sending transport's best-known per-peer message ceiling. The fixed wire
/// header, CID, and overlay envelope share that ceiling with the payload, so
/// their overhead is subtracted here. An unknown or implausibly small limit
/// retains the conservative 16 KiB behavior instead of producing degenerate
/// near-zero chunks.
pub fn chunk_size_for_limit(cid: &str, limit: Option<u32>) -> usize {
    const HAVE_CHUNK_HEADER_LEN: usize = 6;
    /// Storage sends chunks through `OverlayTransport`, which wraps each payload in an
    /// `OverlayEnvelope` before the real transport's size check. The exact overhead depends on
    /// the two `NodeId` lengths, so reserve a generous fixed margin rather than risk a chunk being
    /// silently dropped after wrapping.
    const OVERLAY_ENVELOPE_SAFETY_MARGIN: usize = 512;
    const MIN_CHUNK_SIZE: usize = 1024;

    let Some(limit) = limit else {
        return HAVE_CHUNK_SIZE;
    };
    let overhead = HAVE_CHUNK_HEADER_LEN + cid.len() + OVERLAY_ENVELOPE_SAFETY_MARGIN;
    match (limit as usize).checked_sub(overhead) {
        Some(usable) if usable >= MIN_CHUNK_SIZE => usable,
        _ => HAVE_CHUNK_SIZE,
    }
}

/// Number of `chunk_size`-sized chunks needed to send `data_len` bytes, or
/// `None` if that count would overflow the wire format's `u16` chunk
/// index/total (guards against silently wrapping on an oversized block).
pub fn have_chunk_count(data_len: usize, chunk_size: usize) -> Option<u16> {
    debug_assert!(chunk_size > 0);
    let chunk_count = data_len.div_ceil(chunk_size);
    if chunk_count > u16::MAX as usize {
        None
    } else {
        Some(chunk_count as u16)
    }
}

/// Builds the wire payload for chunk `chunk_index` of `total_chunks`: a
/// single `HAVE` message if the whole block fits in one chunk, otherwise a
/// `HAVE_CHUNK` message carrying just that chunk's slice of `data`.
pub fn build_have_payload(
    cid: &str,
    data: &[u8],
    chunk_index: u16,
    total_chunks: u16,
    chunk_size: usize,
) -> Vec<u8> {
    debug_assert!(chunk_size > 0);
    if total_chunks <= 1 {
        return build_have_message(cid, data);
    }

    let start = (chunk_index as usize) * chunk_size;
    let end = ((chunk_index as usize + 1) * chunk_size).min(data.len());
    build_have_chunk_message(cid, chunk_index, total_chunks, &data[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_wants_do_not_replace_each_other() {
        let registry = WantRegistry::new();
        let mut first = registry.register("cid");
        let mut second = registry.register("cid");
        registry.fulfill("cid", b"payload".to_vec());
        assert_eq!(first.try_recv().unwrap(), b"payload");
        assert_eq!(second.try_recv().unwrap(), b"payload");
    }

    #[test]
    fn unsolicited_chunks_do_not_allocate() {
        let registry = WantRegistry::new();
        registry.fulfill_chunk("never-requested", 0, 65535, vec![1]);
        assert!(registry.state.lock().unwrap().assemblies.is_empty());
    }

    fn peer(name: &str) -> NodeId {
        NodeId(name.to_string())
    }

    #[test]
    fn only_registered_senders_can_fulfill_or_allocate() {
        let registry = WantRegistry::new();
        let mut rx = registry.register_from("cid", peer("wanted"));
        registry.fulfill_chunk_from("cid", &peer("other"), 0, 2, vec![1]);
        registry.fulfill_from("cid", &peer("other"), vec![9]);
        assert!(registry.state.lock().unwrap().assemblies.is_empty());
        assert_eq!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        registry.fulfill_from("cid", &peer("wanted"), vec![7]);
        assert_eq!(rx.try_recv().unwrap(), vec![7]);
    }

    #[test]
    fn sender_assemblies_never_mix_and_metadata_changes_do_not_reset() {
        let registry = WantRegistry::new();
        let mut rx = registry.register_for_peers("cid", vec![peer("a"), peer("b")]);
        registry.fulfill_chunk_from("cid", &peer("a"), 0, 2, b"aa".to_vec());
        registry.fulfill_chunk_from("cid", &peer("b"), 1, 2, b"BB".to_vec());
        registry.fulfill_chunk_from("cid", &peer("a"), 0, 3, b"wrong".to_vec());
        registry.fulfill_chunk_from("cid", &peer("a"), 0, 2, b"duplicate".to_vec());
        assert_eq!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        registry.fulfill_chunk_from("cid", &peer("a"), 1, 2, b"bb".to_vec());
        assert_eq!(rx.try_recv().unwrap(), b"aabb");
        assert!(registry.state.lock().unwrap().assemblies.is_empty());
    }

    #[test]
    fn abandoned_request_does_not_cancel_another_request() {
        let registry = WantRegistry::new();
        let first = registry.register_from("cid", peer("a"));
        let mut second = registry.register_from("cid", peer("b"));
        registry.fulfill_chunk_from("cid", &peer("a"), 0, 2, vec![1]);
        drop(first);
        registry.cancel_closed();
        assert!(registry.state.lock().unwrap().assemblies.is_empty());
        registry.fulfill_from("cid", &peer("a"), vec![2]);
        assert_eq!(second.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        registry.fulfill_from("cid", &peer("b"), vec![3]);
        assert_eq!(second.try_recv().unwrap(), vec![3]);
    }

    #[test]
    fn reassembly_bounds_chunk_count_block_bytes_and_total_bytes() {
        let registry = WantRegistry::new();
        let mut wants = Vec::new();
        for i in 0..=MAX_WANT_ASSEMBLY_BYTES / MAX_WANT_BLOCK_BYTES {
            let cid = format!("cid-{i}");
            wants.push(registry.register(&cid));
            registry.fulfill_chunk(&cid, 0, MAX_WANT_CHUNKS + 1, vec![1]);
            assert!(!registry
                .state
                .lock()
                .unwrap()
                .assemblies
                .contains_key(&(cid.clone(), peer(""))));
            registry.fulfill_chunk(&cid, 0, 2, vec![0; MAX_WANT_BLOCK_BYTES]);
            registry.fulfill_chunk(&cid, 1, 2, vec![1]);
        }
        let state = registry.state.lock().unwrap();
        assert_eq!(
            state
                .assemblies
                .values()
                .map(|entry| entry.bytes)
                .sum::<usize>(),
            MAX_WANT_ASSEMBLY_BYTES
        );
        assert!(state
            .assemblies
            .values()
            .all(|entry| entry.bytes <= MAX_WANT_BLOCK_BYTES));
        assert_eq!(
            state.assemblies.len(),
            MAX_WANT_ASSEMBLY_BYTES / MAX_WANT_BLOCK_BYTES
        );
        drop(state);
        drop(wants);
        registry.cancel_closed();
        assert!(registry.state.lock().unwrap().assemblies.is_empty());
    }

    #[test]
    fn registry_bounds_waiters_and_number_of_assemblies() {
        let registry = WantRegistry::new();
        let mut wants = Vec::new();
        for i in 0..MAX_PENDING_WANTS {
            let cid = format!("cid-{i}");
            wants.push(registry.register(&cid));
            registry.fulfill_chunk(&cid, 0, 2, vec![1]);
        }
        assert_eq!(
            registry.state.lock().unwrap().assemblies.len(),
            MAX_WANT_ASSEMBLIES
        );
        assert_eq!(
            registry.register("overflow").try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
        registry.cancel("cid-0");
        assert_eq!(
            wants[0].try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
        let replacement = registry.register("replacement");
        registry.fulfill_chunk("replacement", 0, 2, vec![2]);
        assert_eq!(
            registry.state.lock().unwrap().assemblies.len(),
            MAX_WANT_ASSEMBLIES
        );
        drop(replacement);
    }

    #[test]
    fn chunk_size_falls_back_when_limit_is_unknown_or_too_small() {
        assert_eq!(chunk_size_for_limit("cid", None), HAVE_CHUNK_SIZE);
        assert_eq!(chunk_size_for_limit("cid", Some(100)), HAVE_CHUNK_SIZE);
    }

    #[test]
    fn chunk_size_uses_realistic_negotiated_limit_without_exceeding_it() {
        const LIMIT: u32 = 262_144;
        const HEADER_LEN: usize = 6;
        const ENVELOPE_MARGIN: usize = 512;
        let cid = "bafy-short-cid";
        let chunk_size = chunk_size_for_limit(cid, Some(LIMIT));

        assert_eq!(
            chunk_size + HEADER_LEN + cid.len() + ENVELOPE_MARGIN,
            LIMIT as usize
        );
    }

    #[test]
    fn measured_limit_still_increases_chunk_size_with_envelope_margin() {
        let chunk_size = chunk_size_for_limit("bafy-short-cid", Some(262_144));

        assert!(chunk_size > HAVE_CHUNK_SIZE);
    }

    #[test]
    fn envelope_margin_affects_minimum_chunk_size_fallback() {
        const HEADER_LEN: usize = 6;
        const ENVELOPE_MARGIN: usize = 512;
        const MIN_CHUNK_SIZE: usize = 1024;
        let cid = "cid";
        let just_below_minimum = HEADER_LEN + cid.len() + ENVELOPE_MARGIN + MIN_CHUNK_SIZE - 1;

        assert_eq!(
            chunk_size_for_limit(cid, Some(just_below_minimum as u32)),
            HAVE_CHUNK_SIZE
        );
    }

    #[test]
    fn variable_chunk_size_round_trips_across_wire_messages() {
        let cid = "bafy-variable-chunks";
        let chunk_size = 7;
        let data: Vec<u8> = (0..31).collect();
        let total_chunks = have_chunk_count(data.len(), chunk_size).expect("block should fit");
        let mut reassembled = Vec::with_capacity(data.len());

        for chunk_index in 0..total_chunks {
            let message = build_have_payload(cid, &data, chunk_index, total_chunks, chunk_size);
            let (parsed_cid, parsed_index, parsed_total, payload) =
                parse_have_chunk_message(&message).expect("chunk message should parse");
            assert_eq!(parsed_cid, cid);
            assert_eq!(parsed_index, chunk_index);
            assert_eq!(parsed_total, total_chunks);
            assert!(payload.len() <= chunk_size);
            reassembled.extend_from_slice(&payload);
        }

        assert_eq!(reassembled, data);
    }
    #[test]
    fn w5_regression_bad_sender_must_not_finish_another_peers_want() {
        use crate::storage::cid::{compute_cid, MULTICODEC_RAW};
        let registry = WantRegistry::new();
        let good = b"verified block".to_vec();
        let cid = compute_cid(&good, MULTICODEC_RAW);
        let a = NodeId("bad-peer".into());
        let b = NodeId("good-peer".into());
        let _rx_a = registry.register_from(&cid, a.clone());
        let mut rx_b = registry.register_from(&cid, b.clone());
        registry.fulfill_from(&cid, &a, b"corrupt block".to_vec());
        registry.fulfill_from(&cid, &b, good.clone());
        assert_eq!(
            rx_b.try_recv().unwrap(),
            good,
            "peer A was never authorized by waiter B; corrupt A must not consume B's request"
        );
    }

    #[test]
    fn completing_one_sender_preserves_other_sender_chunk_progress() {
        let registry = WantRegistry::new();
        let mut a = registry.register_from("cid", peer("a"));
        let mut b = registry.register_from("cid", peer("b"));
        registry.fulfill_chunk_from("cid", &peer("b"), 0, 2, b"good-".to_vec());
        registry.fulfill_chunk_from("cid", &peer("a"), 0, 1, b"corrupt".to_vec());
        assert_eq!(a.try_recv().unwrap(), b"corrupt");
        assert_eq!(b.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        assert_eq!(registry.state.lock().unwrap().assemblies.len(), 1);
        registry.fulfill_chunk_from("cid", &peer("b"), 1, 2, b"block".to_vec());
        assert_eq!(b.try_recv().unwrap(), b"good-block");
        assert!(registry.state.lock().unwrap().assemblies.is_empty());
    }
}
