use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, watch};

use crate::types::NodeId;
use web_time::Instant;

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
type RoomPeerNotifiers = Arc<Mutex<HashMap<(String, String), Vec<oneshot::Sender<NodeId>>>>>;

/// Counts are retained by the attempt even after its registration completes.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChunkProgress {
    pub received: u64,
    pub accepted: u64,
    pub last_accepted: Option<Instant>,
}

pub struct WantAttempt {
    pub id: u64,
    pub data: oneshot::Receiver<Vec<u8>>,
    pub resets: watch::Receiver<u64>,
    pub progress: watch::Receiver<ChunkProgress>,
}

struct PendingWant {
    id: u64,
    paths: HashSet<String>,
    resets: watch::Sender<u64>,
    progress: watch::Sender<ChunkProgress>,
    peers: Vec<NodeId>,
    tx: oneshot::Sender<Vec<u8>>,
    byte_limit: Option<(usize, oneshot::Sender<usize>)>,
}

struct ChunkAssembly {
    total_chunks: u16,
    received_chunks: u16,
    bytes: usize,
    chunks: Vec<Option<Vec<u8>>>,
}

struct WantState {
    pending: HashMap<String, Vec<PendingWant>>,
    assemblies: HashMap<(String, NodeId, String), ChunkAssembly>,
    next_attempt: u64,
    progress: HashMap<NodeId, watch::Sender<u64>>,
    sender_activity: HashMap<NodeId, Instant>,
    any_progress: watch::Sender<u64>,
    query_paths: HashMap<(String, NodeId), watch::Sender<u64>>,
}

impl Default for WantState {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            assemblies: HashMap::new(),
            progress: HashMap::new(),
            next_attempt: 0,
            sender_activity: HashMap::new(),
            any_progress: watch::channel(0).0,
            query_paths: HashMap::new(),
        }
    }
}

impl WantState {
    fn accepts(&self, cid: &str, sender: &NodeId, room: &str) -> bool {
        self.pending.get(cid).is_some_and(|wants| {
            wants.iter().any(|want| {
                !want.tx.is_closed() && want.peers.contains(sender) && want.paths.contains(room)
            })
        })
    }

    fn prune(&mut self) {
        self.query_paths
            .retain(|_, resets| resets.receiver_count() > 0);
        self.pending.retain(|_, wants| {
            wants.retain(|want| !want.tx.is_closed());
            !wants.is_empty()
        });
        let pending = &self.pending;
        self.assemblies.retain(|(cid, sender, room), _| {
            pending.get(cid).is_some_and(|wants| {
                wants
                    .iter()
                    .any(|want| want.peers.contains(sender) && want.paths.contains(room))
            })
        });
        if !self.progress.is_empty() {
            let senders: HashSet<_> = pending
                .values()
                .flatten()
                .flat_map(|want| &want.peers)
                .collect();
            self.progress.retain(|sender, _| senders.contains(sender));
            self.sender_activity
                .retain(|sender, _| senders.contains(sender));
        }
    }

    fn observe(
        &self,
        cid: &str,
        sender: &NodeId,
        room: &str,
        accepted: bool,
    ) -> Option<(u64, ChunkProgress)> {
        let mut snapshot = None;
        if let Some(wants) = self.pending.get(cid) {
            for want in wants
                .iter()
                .filter(|want| want.peers.contains(sender) && want.paths.contains(room))
            {
                want.progress.send_modify(|progress| {
                    if accepted {
                        progress.accepted += 1;
                        progress.last_accepted = Some(Instant::now());
                    } else {
                        progress.received += 1;
                    }
                });
                snapshot.get_or_insert((want.id, *want.progress.borrow()));
            }
        }
        snapshot
    }

    fn advance_progress(&mut self, sender: &NodeId) {
        self.sender_activity.insert(sender.clone(), Instant::now());
        self.any_progress
            .send_modify(|counter| *counter = counter.wrapping_add(1));
        self.progress
            .entry(sender.clone())
            .or_insert_with(|| watch::channel(0).0)
            .send_modify(|counter| *counter = counter.wrapping_add(1));
    }

    // Fail only bounded callers on this path; unrelated storage_get waiters
    // keep their original admission policy and partial data.
    fn enforce_byte_limits(&mut self, cid: &str, sender: &NodeId, room: &str, bytes: usize) {
        if let Some(wants) = self.pending.get_mut(cid) {
            let mut index = 0;
            while index < wants.len() {
                let want = &wants[index];
                if want.peers.contains(sender)
                    && want.paths.contains(room)
                    && want
                        .byte_limit
                        .as_ref()
                        .is_some_and(|(limit, _)| bytes > *limit)
                {
                    let want = wants.remove(index);
                    if let Some((_, error)) = want.byte_limit {
                        let _ = error.send(bytes);
                    }
                } else {
                    index += 1;
                }
            }
        }
        self.prune();
    }

    fn fulfill(&mut self, cid: &str, sender: &NodeId, room: &str, data: Vec<u8>) {
        if let Some(wants) = self.pending.remove(cid) {
            // Unverified bytes may only complete waiters that requested this sender.
            let (authorized, remaining): (Vec<_>, Vec<_>) = wants
                .into_iter()
                .partition(|want| want.peers.contains(sender) && want.paths.contains(room));
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
    counters: Arc<super::telemetry::StorageCounters>,
    peer_cache: PeerCache,
    peer_notifiers: PeerNotifiers,
    room_peer_notifiers: RoomPeerNotifiers,
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
            counters: super::telemetry::STORAGE_COUNTERS.clone(),
            peer_cache: Arc::new(Mutex::new(HashMap::new())),
            peer_notifiers: Arc::new(Mutex::new(HashMap::new())),
            room_peer_notifiers: Arc::new(Mutex::new(HashMap::new())),
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
    pub fn register_for_peers(&self, cid: &str, peers: Vec<NodeId>) -> oneshot::Receiver<Vec<u8>> {
        self.register_attempt_for_peers(cid, peers, true).data
    }

    /// Register one resolver attempt before routing WANTs. Paths are attached
    /// immediately before each send, so even teardown during enqueue is retained.
    pub fn register_attempt(&self, cid: &str, sender: NodeId) -> WantAttempt {
        self.register_attempt_for_peers(cid, vec![sender], false)
    }

    /// A resolver attempt with a per-caller payload limit. The error receiver
    /// reports the observed size before that payload is copied or stored.
    /// The registry's existing block bound remains an additional ceiling.
    pub fn register_bounded_attempt(
        &self,
        cid: &str,
        sender: NodeId,
        max_bytes: usize,
    ) -> (WantAttempt, oneshot::Receiver<usize>) {
        let attempt = self.register_attempt(cid, sender);
        let (tx, rx) = oneshot::channel();
        let mut state = self.state.lock().unwrap();
        if let Some(want) = state
            .pending
            .values_mut()
            .flatten()
            .find(|w| w.id == attempt.id)
        {
            want.byte_limit = Some((max_bytes.min(MAX_WANT_BLOCK_BYTES), tx));
        }
        (attempt, rx)
    }

    fn register_attempt_for_peers(
        &self,
        cid: &str,
        mut peers: Vec<NodeId>,
        local: bool,
    ) -> WantAttempt {
        let (tx, rx) = oneshot::channel();
        let (resets, reset_rx) = watch::channel(0);
        let (progress, progress_rx) = watch::channel(ChunkProgress::default());
        let mut state = self.state.lock().unwrap();
        state.prune();
        state.next_attempt = state.next_attempt.wrapping_add(1);
        let id = state.next_attempt;
        let attempt = WantAttempt {
            id,
            data: rx,
            resets: reset_rx,
            progress: progress_rx,
        };
        if cid.len() > u8::MAX as usize
            || peers.is_empty()
            || peers.len() > MAX_WANT_PEERS
            || peers.iter().any(|peer| peer.0.len() > 256)
            || state.pending.values().map(Vec::len).sum::<usize>() >= MAX_PENDING_WANTS
        {
            return attempt;
        }
        peers.sort_by(|a, b| a.0.cmp(&b.0));
        peers.dedup();
        state
            .pending
            .entry(cid.to_string())
            .or_default()
            .push(PendingWant {
                id,
                paths: if local {
                    HashSet::from([String::new()])
                } else {
                    HashSet::new()
                },
                resets,
                progress,
                peers,
                tx,
                byte_limit: None,
            });
        attempt
    }

    pub fn track_path(&self, attempt: u64, room: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        state
            .pending
            .values_mut()
            .flatten()
            .find(|want| want.id == attempt)
            .is_some_and(|want| want.paths.insert(room.to_string()))
    }

    /// Undo a newly attached path when enqueue fails. Paths from earlier
    /// successful sends stay attached until the attempt ends.
    pub fn untrack_path(&self, attempt: u64, room: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(want) = state
            .pending
            .values_mut()
            .flatten()
            .find(|want| want.id == attempt)
        {
            want.paths.remove(room);
        }
        state.prune();
    }

    pub fn attempt_paths(&self, attempt: u64) -> Vec<String> {
        let state = self.state.lock().unwrap();
        let mut paths: Vec<_> = state
            .pending
            .values()
            .flatten()
            .find(|want| want.id == attempt)
            .map(|want| want.paths.iter().cloned().collect())
            .unwrap_or_default();
        paths.sort();
        paths
    }

    pub fn sender_last_accepted(&self, sender: &NodeId) -> Option<Instant> {
        self.state
            .lock()
            .unwrap()
            .sender_activity
            .get(sender)
            .copied()
    }

    pub fn fulfill(&self, cid: &str, data: Vec<u8>) {
        self.fulfill_from(cid, &NodeId(String::new()), data);
    }

    pub fn fulfill_from(&self, cid: &str, sender: &NodeId, data: Vec<u8>) {
        self.fulfill_in("", cid, sender, data);
    }

    pub fn fulfill_in(&self, room: &str, cid: &str, sender: &NodeId, data: Vec<u8>) {
        self.fulfill_data_in(room, cid, sender, Cow::Owned(data));
    }

    /// Network receive path: apply limits before copying the borrowed payload.
    pub fn fulfill_slice_in(&self, room: &str, cid: &str, sender: &NodeId, data: &[u8]) {
        self.fulfill_data_in(room, cid, sender, Cow::Borrowed(data));
    }

    fn fulfill_data_in(&self, room: &str, cid: &str, sender: &NodeId, data: Cow<'_, [u8]>) {
        let mut state = self.state.lock().unwrap();
        state.prune();
        let counters = state.observe(cid, sender, room, false);
        state.enforce_byte_limits(cid, sender, room, data.len());
        let accepted = data.len() <= MAX_WANT_BLOCK_BYTES && state.accepts(cid, sender, room);
        tracing::debug!(%cid, peer = %sender.0, %room, len = data.len(), accepted, ?counters,
            reason = if accepted { "complete" } else { "oversized or no WANT on path" }, "Storage: HAVE received");
        if accepted {
            self.counters.accepted(data.len());
            self.counters.completed();
            let counters = state.observe(cid, sender, room, true);
            tracing::debug!(%cid, peer = %sender.0, %room, ?counters, "Storage: HAVE accepted; response complete");
            state.advance_progress(sender);
            state.fulfill(cid, sender, room, data.into_owned());
        } else {
            self.counters.rejected();
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
        self.fulfill_chunk_in("", cid, sender, chunk_index, chunk_total, data);
    }

    pub fn fulfill_chunk_in(
        &self,
        room: &str,
        cid: &str,
        sender: &NodeId,
        chunk_index: u16,
        chunk_total: u16,
        data: Vec<u8>,
    ) {
        self.fulfill_chunk_data_in(
            room,
            cid,
            sender,
            chunk_index,
            chunk_total,
            Cow::Owned(data),
        );
    }

    /// Network receive path: no payload allocation precedes byte admission.
    pub fn fulfill_chunk_slice_in(
        &self,
        room: &str,
        cid: &str,
        sender: &NodeId,
        chunk_index: u16,
        chunk_total: u16,
        data: &[u8],
    ) {
        self.fulfill_chunk_data_in(
            room,
            cid,
            sender,
            chunk_index,
            chunk_total,
            Cow::Borrowed(data),
        );
    }

    fn fulfill_chunk_data_in(
        &self,
        room: &str,
        cid: &str,
        sender: &NodeId,
        chunk_index: u16,
        chunk_total: u16,
        data: Cow<'_, [u8]>,
    ) {
        // One lock makes observation, admission, completion and reset atomic.
        let mut state = self.state.lock().unwrap();
        state.prune();
        let counters = state.observe(cid, sender, room, false);
        tracing::debug!(%cid, peer = %sender.0, %room, chunk_index, chunk_total, len = data.len(), ?counters, "Storage: chunk received");
        if chunk_total == 0 || chunk_total > MAX_WANT_CHUNKS || chunk_index >= chunk_total {
            tracing::debug!(%cid, %room, chunk_index, reason = "invalid header or oversized chunk", "Storage: chunk rejected");
            self.counters.rejected();
            return;
        }
        if !state.accepts(cid, sender, room) {
            tracing::debug!(%cid, peer = %sender.0, %room, chunk_index, reason = "no WANT on path", "Storage: chunk rejected");
            self.counters.rejected();
            return;
        }
        let key = (cid.to_string(), sender.clone(), room.to_string());
        if let Some(entry) = state.assemblies.get(&key) {
            // Never reset an assembly in response to contradictory metadata.
            if entry.total_chunks != chunk_total {
                tracing::debug!(%cid, peer = %sender.0, %room, chunk_index, expected_total = entry.total_chunks, chunk_total, reason = "contradictory total", "Storage: chunk rejected");
                self.counters.rejected();
                return;
            }
            if entry.chunks[chunk_index as usize].is_some() {
                tracing::debug!(%cid, peer = %sender.0, %room, chunk_index, reason = "duplicate index", "Storage: chunk duplicate");
                self.counters.duplicate(data.len());
                return;
            }
        }
        let bytes = state
            .assemblies
            .get(&key)
            .map_or(0, |entry| entry.bytes)
            .saturating_add(data.len());
        state.enforce_byte_limits(cid, sender, room, bytes);
        if !state.accepts(cid, sender, room) || bytes > MAX_WANT_BLOCK_BYTES {
            self.counters.rejected();
            return;
        }
        if !state.assemblies.contains_key(&key) && state.assemblies.len() >= MAX_WANT_ASSEMBLIES {
            tracing::debug!(%cid, %room, chunk_index, reason = "assembly count limit", "Storage: chunk rejected");
            self.counters.rejected();
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
            tracing::debug!(%cid, %room, chunk_index, reason = "registry byte limit", "Storage: chunk rejected");
            self.counters.rejected();
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
        self.counters.accepted(data.len());
        entry.bytes += data.len();
        entry.chunks[chunk_index as usize] = Some(data.into_owned());
        entry.received_chunks += 1;
        let complete = entry.received_chunks == entry.total_chunks;
        let assembly_received = entry.received_chunks;
        let bytes = entry.bytes;
        let counters = state.observe(cid, sender, room, true);
        tracing::debug!(%cid, peer = %sender.0, %room, chunk_index, assembly_received, total = chunk_total, bytes, complete, ?counters, "Storage: chunk accepted");
        state.advance_progress(sender);
        if complete {
            self.counters.completed();
            let entry = state.assemblies.get_mut(&key).unwrap();
            let mut full = Vec::with_capacity(entry.bytes);
            for part in &mut entry.chunks {
                full.extend_from_slice(&part.take().unwrap());
            }
            state.assemblies.remove(&key);
            state.fulfill(cid, sender, room, full);
        }
    }

    /// Remove only abandoned waiters, preserving other concurrent requests.
    pub fn cancel_closed(&self) {
        self.state.lock().unwrap().prune();
        self.peer_notifiers.lock().unwrap().retain(|_, senders| {
            senders.retain(|tx| !tx.is_closed());
            !senders.is_empty()
        });
        self.room_peer_notifiers
            .lock()
            .unwrap()
            .retain(|_, senders| {
                senders.retain(|tx| !tx.is_closed());
                !senders.is_empty()
            });
    }

    /// Senders with open WANT registrations, excluding abandoned or completed requests.
    /// Prune with the same rules as cancellation before taking the snapshot.
    pub fn outstanding_senders(&self) -> Vec<NodeId> {
        let mut state = self.state.lock().unwrap();
        state.prune();
        state
            .pending
            .values()
            .flatten()
            .flat_map(|want| want.peers.iter().cloned())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// Subscribe after registering a WANT and before sending it. Accepted data for
    /// any CID from this sender advances the counter, including between polls.
    /// Unreferenced senders get a closed receiver without allocating a map entry;
    /// live entries are bounded by the admitted wants and their candidate peers.
    pub fn progress(&self, sender: &NodeId) -> watch::Receiver<u64> {
        let mut state = self.state.lock().unwrap();
        state.prune();
        if !state
            .pending
            .values()
            .flatten()
            .any(|want| want.peers.contains(sender))
        {
            return watch::channel(0).1;
        }
        state
            .progress
            .entry(sender.clone())
            .or_insert_with(|| watch::channel(0).0)
            .subscribe()
    }

    /// Confirmed path teardown, never temporary ICE suspicion. Keep registrations
    /// for replay, and clear only that room's assemblies (layouts may change).
    pub fn sender_reset(&self, sender: &NodeId, room: &str) {
        self.reset_path(Some(sender), room);
    }

    /// A whole session is gone, including routed paths whose next-hop mapping
    /// may already have been removed with the session. Notify each attempt once.
    pub fn room_reset(&self, room: &str) {
        self.reset_path(None, room);
    }

    /// Observe a QUERY recipient before enqueue, without registering a WANT or
    /// accepting bulk responses. Only live discovery observers retain entries.
    pub(crate) fn query_path_resets(&self, room: &str, peer: &NodeId) -> watch::Receiver<u64> {
        let mut state = self.state.lock().unwrap();
        state.prune();
        state
            .query_paths
            .entry((room.to_string(), peer.clone()))
            .or_insert_with(|| watch::channel(0).0)
            .subscribe()
    }

    fn reset_path(&self, sender: Option<&NodeId>, room: &str) {
        let mut state = self.state.lock().unwrap();
        state.prune();
        for ((path, peer), resets) in &state.query_paths {
            if path == room && sender.is_none_or(|sender| sender == peer) {
                resets.send_modify(|counter| *counter = counter.wrapping_add(1));
            }
        }
        let before = state.assemblies.len();
        state.assemblies.retain(|(_, source, path), _| {
            path != room || sender.is_some_and(|sender| source != sender)
        });
        let mut affected = 0;
        for want in state.pending.values().flatten() {
            if sender.is_none_or(|sender| want.peers.contains(sender)) && want.paths.contains(room)
            {
                want.resets
                    .send_modify(|counter| *counter = counter.wrapping_add(1));
                affected += 1;
                tracing::debug!(attempt = want.id, ?sender, %room, generation = *want.resets.borrow(), "Storage: attempt path reset");
            }
        }
        tracing::debug!(?sender, %room, affected, assemblies = before - state.assemblies.len(), "Storage: path reset");
    }

    /// Subscribe before discovery broadcasts. Every accepted, stored HAVE or
    /// HAVE_CHUNK advances this counter, regardless of CID or sender. A single
    /// watch value retains progress between polls without per-discovery state.
    pub fn any_progress(&self) -> watch::Receiver<u64> {
        self.state.lock().unwrap().any_progress.subscribe()
    }

    /// Explicitly cancel all requests for a CID. A single resolver timeout must not use this.
    pub fn cancel(&self, cid: &str) {
        let mut state = self.state.lock().unwrap();
        state.pending.remove(cid);
        state.assemblies.retain(|(key, _, _), _| key != cid);
        state.prune();
        self.peer_notifiers.lock().unwrap().remove(cid);
        self.room_peer_notifiers
            .lock()
            .unwrap()
            .retain(|(_, key), _| key != cid);
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

    /// Subscribe before QUERY, without consulting the process-wide peer cache.
    /// Only HAVE_STATUS received in this room can complete this discovery.
    pub fn register_peer_notifier_in(&self, room: &str, cid: &str) -> oneshot::Receiver<NodeId> {
        let (tx, rx) = oneshot::channel();
        self.cancel_closed();
        let mut notifiers = self.room_peer_notifiers.lock().unwrap();
        if notifiers.values().map(Vec::len).sum::<usize>() < MAX_PENDING_WANTS {
            notifiers
                .entry((room.to_string(), cid.to_string()))
                .or_default()
                .push(tx);
        }
        rx
    }

    /// Preserve legacy resolver notifications while also notifying room-scoped discovery.
    pub fn register_peer_in(&self, room: &str, cid: &str, peer: NodeId) {
        if let Some(notifiers) = self
            .room_peer_notifiers
            .lock()
            .unwrap()
            .remove(&(room.to_string(), cid.to_string()))
        {
            for tx in notifiers {
                let _ = tx.send(peer.clone());
            }
        }
        self.register_peer(cid, peer);
    }

    pub fn register_peer(&self, cid: &str, peer_id: NodeId) {
        self.counters
            .have_status_received
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

/// A validated storage frame borrowing its CID and payload from the wire bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageFrame<'a> {
    Want {
        cid: &'a str,
    },
    Have {
        cid: &'a str,
        data: &'a [u8],
    },
    Query {
        cid: &'a str,
    },
    HaveStatus {
        cid: &'a str,
    },
    HaveChunk {
        cid: &'a str,
        chunk_index: u16,
        chunk_total: u16,
        data: &'a [u8],
    },
}

/// Classifies only well-formed storage frames with a valid CID. Other raw
/// bytes belong to the application, even if their first byte is a storage tag.
/// This checks framing, not whether HAVE bytes hash to the advertised CID.
pub fn classify_storage_frame(raw: &[u8]) -> Option<StorageFrame<'_>> {
    fn parse_cid(bytes: &[u8]) -> Option<&str> {
        if bytes.len() > u8::MAX as usize {
            return None;
        }
        let cid = std::str::from_utf8(bytes).ok()?;
        crate::storage::cid::is_valid_cid(cid).then_some(cid)
    }

    let (&tag, rest) = raw.split_first()?;
    match tag {
        MSG_WANT => Some(StorageFrame::Want {
            cid: parse_cid(rest)?,
        }),
        MSG_QUERY => Some(StorageFrame::Query {
            cid: parse_cid(rest)?,
        }),
        MSG_HAVE_STATUS => Some(StorageFrame::HaveStatus {
            cid: parse_cid(rest)?,
        }),
        MSG_HAVE => {
            let (&cid_len, body) = rest.split_first()?;
            let cid_len = cid_len as usize;
            let cid = parse_cid(body.get(..cid_len)?)?;
            Some(StorageFrame::Have {
                cid,
                data: &body[cid_len..],
            })
        }
        MSG_HAVE_CHUNK => {
            let header = raw.get(..6)?;
            let cid_len = header[1] as usize;
            let chunk_index = u16::from_be_bytes([header[2], header[3]]);
            let chunk_total = u16::from_be_bytes([header[4], header[5]]);
            if chunk_total == 0 || chunk_total > MAX_WANT_CHUNKS || chunk_index >= chunk_total {
                return None;
            }
            let cid = parse_cid(raw.get(6..6 + cid_len)?)?;
            Some(StorageFrame::HaveChunk {
                cid,
                chunk_index,
                chunk_total,
                data: &raw[6 + cid_len..],
            })
        }
        _ => None,
    }
}

pub fn parse_have_message(raw: &[u8]) -> Option<(String, Vec<u8>)> {
    match classify_storage_frame(raw)? {
        StorageFrame::Have { cid, data } => Some((cid.to_string(), data.to_vec())),
        _ => None,
    }
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
    match classify_storage_frame(raw)? {
        StorageFrame::HaveChunk {
            cid,
            chunk_index,
            chunk_total,
            data,
        } => Some((cid.to_string(), chunk_index, chunk_total, data.to_vec())),
        _ => None,
    }
}

pub fn parse_want_message(raw: &[u8]) -> Option<String> {
    match classify_storage_frame(raw)? {
        StorageFrame::Want { cid } => Some(cid.to_string()),
        _ => None,
    }
}

pub fn build_want_message(cid: &str) -> Vec<u8> {
    let mut msg = vec![MSG_WANT];
    msg.extend_from_slice(cid.as_bytes());
    msg
}

pub fn build_query_message(cid: &str) -> Vec<u8> {
    let mut msg = vec![MSG_QUERY];
    msg.extend_from_slice(cid.as_bytes());
    msg
}

pub fn parse_query_message(raw: &[u8]) -> Option<String> {
    match classify_storage_frame(raw)? {
        StorageFrame::Query { cid } => Some(cid.to_string()),
        _ => None,
    }
}

pub fn build_have_status_message(cid: &str) -> Vec<u8> {
    let mut msg = vec![MSG_HAVE_STATUS];
    msg.extend_from_slice(cid.as_bytes());
    msg
}

pub fn parse_have_status_message(raw: &[u8]) -> Option<String> {
    match classify_storage_frame(raw)? {
        StorageFrame::HaveStatus { cid } => Some(cid.to_string()),
        _ => None,
    }
}

/// Picks the wire chunk size for one `cid`'s `HAVE_CHUNK` sends from the
/// sending transport's best-known per-peer message ceiling. The fixed wire
/// header, CID, and overlay envelope share that ceiling with the payload, so
/// their overhead is subtracted here. Payloads never exceed `HAVE_CHUNK_SIZE`:
/// a negotiated ceiling is a legality bound, not an efficient SCTP message size.
/// An unknown or implausibly small limit
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
        Some(usable) if usable >= MIN_CHUNK_SIZE => usable.min(HAVE_CHUNK_SIZE),
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
    fn bounded_attempt_rejects_before_payload_storage_and_cleans_assemblies() {
        let registry = WantRegistry::new();
        let sender = peer("bounded");
        let (mut attempt, mut exceeded) =
            registry.register_bounded_attempt("cid", sender.clone(), 4);
        registry.track_path(attempt.id, "room");
        registry.fulfill_slice_in("other", "cid", &sender, b"oversize");
        registry.fulfill_slice_in("room", "cid", &peer("wrong"), b"oversize");
        assert!(exceeded.try_recv().is_err());
        registry.fulfill_chunk_slice_in("room", "cid", &sender, 0, 2, b"123");
        // Duplicates and contradictory totals do not consume the caller's budget.
        registry.fulfill_chunk_slice_in("room", "cid", &sender, 0, 2, b"123");
        registry.fulfill_chunk_slice_in("room", "cid", &sender, 1, 3, b"ignored");
        assert!(exceeded.try_recv().is_err());
        registry.fulfill_chunk_slice_in("room", "cid", &sender, 1, 2, b"45");
        assert_eq!(exceeded.try_recv().unwrap(), 5);
        assert!(attempt.data.try_recv().is_err());
        let state = registry.state.lock().unwrap();
        assert!(state.pending.is_empty());
        assert!(state.assemblies.is_empty());
        drop(state);

        // Ordinary HAVE and an individual huge chunk fail without an assembly.
        for chunks in [false, true] {
            let (attempt, mut exceeded) =
                registry.register_bounded_attempt("cid", sender.clone(), 0);
            registry.track_path(attempt.id, "room");
            if chunks {
                registry.fulfill_chunk_slice_in("room", "cid", &sender, 0, MAX_WANT_CHUNKS, b"x");
            } else {
                registry.fulfill_slice_in("room", "cid", &sender, b"x");
            }
            assert_eq!(exceeded.try_recv().unwrap(), 1);
            assert!(registry.state.lock().unwrap().assemblies.is_empty());
        }
        let (attempt, mut exceeded) = registry.register_bounded_attempt("empty", sender.clone(), 0);
        registry.track_path(attempt.id, "room");
        registry.fulfill_slice_in("room", "empty", &sender, b"");
        assert_eq!(attempt.data.blocking_recv().unwrap(), b"");
        assert!(exceeded.try_recv().is_err());
    }

    #[test]
    fn room_discovery_is_independent_bounded_and_prunes_closed_receivers() {
        let registry = WantRegistry::new();
        let mut wanted = registry.register_peer_notifier_in("room", "cid");
        let mut other = registry.register_peer_notifier_in("other", "cid");
        registry.register_peer_in("room", "cid", peer("provider"));
        assert_eq!(wanted.try_recv().unwrap(), peer("provider"));
        assert!(other.try_recv().is_err());
        drop(other);
        registry.cancel_closed();
        assert!(registry.room_peer_notifiers.lock().unwrap().is_empty());
        let waiters: Vec<_> = (0..MAX_PENDING_WANTS)
            .map(|_| registry.register_peer_notifier_in("room", "cid"))
            .collect();
        let mut rejected = registry.register_peer_notifier_in("room", "cid");
        assert_eq!(
            rejected.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
        drop(waiters);
        registry.cancel_closed();
        assert!(registry.room_peer_notifiers.lock().unwrap().is_empty());
    }
    use crate::storage::cid::{compute_cid, MULTICODEC_RAW};

    #[test]
    fn telemetry_counts_unique_duplicate_and_rejected_admission() {
        let counters = Arc::new(super::super::telemetry::StorageCounters::default());
        let registry = WantRegistry {
            counters: counters.clone(),
            ..WantRegistry::new()
        };
        let sender = NodeId("peer".into());
        let _want = registry.register_from("cid", sender.clone());
        registry.fulfill_chunk_from("cid", &sender, 0, 2, vec![1, 2]);
        registry.fulfill_chunk_from("cid", &sender, 0, 2, vec![1, 2]);
        registry.fulfill_chunk_from("cid", &sender, 1, 3, vec![3]);
        registry.fulfill_chunk_from("cid", &sender, 2, 2, vec![3]);
        registry.fulfill_chunk_from("unknown", &sender, 0, 1, vec![3]);
        registry.fulfill_chunk_from("cid", &sender, 1, 2, vec![3]);
        let s = counters.snapshot();
        assert_eq!((s.accepted_chunks, s.accepted_bytes), (2, 3));
        assert_eq!((s.duplicate_chunks, s.duplicate_bytes), (1, 2));
        assert_eq!(s.rejected_chunks, 3);
        assert_eq!(s.blocks_completed, 1);
        let _want = registry.register_from("whole", sender.clone());
        registry.fulfill_from("whole", &sender, vec![4, 5]);
        registry.fulfill_from("unsolicited", &sender, vec![4]);
        let s = counters.snapshot();
        assert_eq!((s.accepted_chunks, s.accepted_bytes), (3, 5));
        assert_eq!(s.rejected_chunks, 4);
        assert_eq!(s.blocks_completed, 2);
    }

    #[test]
    fn classifier_accepts_all_storage_builders_and_borrows_payloads() {
        let data = b"storage payload";
        let cid = compute_cid(data, MULTICODEC_RAW);
        let frames = [
            (build_want_message(&cid), StorageFrame::Want { cid: &cid }),
            (build_query_message(&cid), StorageFrame::Query { cid: &cid }),
            (
                build_have_status_message(&cid),
                StorageFrame::HaveStatus { cid: &cid },
            ),
            (
                build_have_message(&cid, data),
                StorageFrame::Have { cid: &cid, data },
            ),
            (
                build_have_chunk_message(&cid, 1, 2, data),
                StorageFrame::HaveChunk {
                    cid: &cid,
                    chunk_index: 1,
                    chunk_total: 2,
                    data,
                },
            ),
        ];
        for (raw, expected) in frames {
            assert_eq!(classify_storage_frame(&raw), Some(expected));
            match classify_storage_frame(&raw).unwrap() {
                StorageFrame::Have {
                    cid: parsed_cid,
                    data: parsed_data,
                } => {
                    assert_eq!(parsed_cid.as_ptr(), raw[2..].as_ptr());
                    assert_eq!(parsed_data.as_ptr(), raw[2 + cid.len()..].as_ptr());
                }
                StorageFrame::HaveChunk {
                    cid: parsed_cid,
                    data: parsed_data,
                    ..
                } => {
                    assert_eq!(parsed_cid.as_ptr(), raw[6..].as_ptr());
                    assert_eq!(parsed_data.as_ptr(), raw[6 + cid.len()..].as_ptr());
                }
                _ => {}
            }
        }
        assert_eq!(
            parse_want_message(&build_want_message(&cid)),
            Some(cid.clone())
        );
        assert_eq!(
            parse_query_message(&build_query_message(&cid)),
            Some(cid.clone())
        );
        assert_eq!(
            parse_have_status_message(&build_have_status_message(&cid)),
            Some(cid.clone())
        );
        assert_eq!(
            parse_have_message(&build_have_message(&cid, data)),
            Some((cid.clone(), data.to_vec()))
        );
        assert_eq!(
            parse_have_chunk_message(&build_have_chunk_message(&cid, 1, 2, data)),
            Some((cid, 1, 2, data.to_vec()))
        );
    }

    fn assert_application_frame(raw: &[u8]) {
        assert_eq!(classify_storage_frame(raw), None, "raw: {raw:?}");
        assert!(parse_want_message(raw).is_none());
        assert!(parse_query_message(raw).is_none());
        assert!(parse_have_status_message(raw).is_none());
        assert!(parse_have_message(raw).is_none());
        assert!(parse_have_chunk_message(raw).is_none());
    }

    #[test]
    fn classifier_rejects_application_bytes_and_invalid_cids() {
        for raw in [
            b"".as_slice(),
            b"\x01hello",
            b"\x03",
            b"\xff\x00\x93\x17",
            b"application data",
        ] {
            assert_application_frame(raw);
        }
        for cid in [
            "".to_string(),
            "hello".into(),
            "../../etc/passwd".into(),
            "bABC".into(),
            "bhello0".into(),
            format!("b{}", "a".repeat(128)),
            format!("b{}", "a".repeat(255)),
        ] {
            for raw in [
                build_want_message(&cid),
                build_query_message(&cid),
                build_have_status_message(&cid),
                build_have_message(&cid, b"data"),
                build_have_chunk_message(&cid, 0, 1, b"data"),
            ] {
                assert_application_frame(&raw);
            }
        }
        for tag in [MSG_WANT, MSG_QUERY, MSG_HAVE_STATUS] {
            assert_application_frame(&[tag, 0xff]);
            let cid = compute_cid(b"cid", MULTICODEC_RAW);
            let mut raw = [vec![tag], cid.into_bytes()].concat();
            raw.push(0); // Trailing bytes cannot be ignored on CID-only frames.
            assert_application_frame(&raw);
        }
        assert_application_frame(&[MSG_HAVE, 1, 0xff]);
        assert_application_frame(&[MSG_HAVE_CHUNK, 1, 0, 0, 0, 1, 0xff]);
    }

    #[test]
    fn classifier_rejects_truncated_frames_and_invalid_chunk_headers() {
        let cid = compute_cid(b"chunk", MULTICODEC_RAW);
        for raw in [
            build_have_message(&cid, b""),
            build_have_chunk_message(&cid, 0, 1, b""),
        ] {
            for end in 0..raw.len() {
                assert_application_frame(&raw[..end]);
            }
            assert!(classify_storage_frame(&raw).is_some());
        }
        for (index, total) in [
            (0, 0),
            (1, 1),
            (2, 1),
            (0, MAX_WANT_CHUNKS + 1),
            (0, u16::MAX),
        ] {
            assert_application_frame(&build_have_chunk_message(&cid, index, total, b"data"));
        }
        assert!(classify_storage_frame(&build_have_chunk_message(
            &cid,
            MAX_WANT_CHUNKS - 1,
            MAX_WANT_CHUNKS,
            b"data"
        ))
        .is_some());
    }

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
    fn room_reset_preserves_same_cid_sender_assembly_and_waiter_in_other_room() {
        let registry = WantRegistry::new();
        let sender = peer("shared-provider");
        let mut main = registry.register_attempt("cid", sender.clone());
        let mut secondary = registry.register_attempt("cid", sender.clone());
        registry.track_path(main.id, "main");
        registry.track_path(secondary.id, "secondary");
        registry.fulfill_chunk_in("main", "cid", &sender, 0, 2, b"main-".to_vec());
        registry.fulfill_chunk_in("secondary", "cid", &sender, 0, 3, b"old".to_vec());
        let progress = registry.progress(&sender);
        let before = *progress.borrow();
        registry.sender_reset(&sender, "secondary");
        assert!(!main.resets.has_changed().unwrap());
        assert_eq!(*secondary.resets.borrow_and_update(), 1);
        assert_eq!(*progress.borrow(), before, "reset is not storage progress");
        assert_eq!(registry.state.lock().unwrap().assemblies.len(), 1);
        registry.fulfill_chunk_in("main", "cid", &sender, 1, 2, b"block".to_vec());
        assert_eq!(main.data.try_recv().unwrap(), b"main-block");
        assert_eq!(
            secondary.data.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );
        // Same-room replay may change layout; it still completes independently.
        registry.fulfill_chunk_in("secondary", "cid", &sender, 0, 1, b"replacement".to_vec());
        assert_eq!(secondary.data.try_recv().unwrap(), b"replacement");
        assert!(registry.state.lock().unwrap().assemblies.is_empty());
        assert!(registry.state.lock().unwrap().sender_activity.is_empty());
    }

    #[test]
    fn whole_room_reset_covers_all_providers_once_and_preserves_other_room() {
        let registry = WantRegistry::new();
        let main = registry.register_attempt("main-cid", peer("a"));
        registry.track_path(main.id, "main");
        let first =
            registry.register_attempt_for_peers("first", vec![peer("a"), peer("relay")], false);
        let second = registry.register_attempt("second", peer("routed-provider"));
        registry.track_path(first.id, "secondary");
        registry.track_path(second.id, "secondary");
        registry.fulfill_chunk_in("main", "main-cid", &peer("a"), 0, 2, vec![1]);
        registry.fulfill_chunk_in("secondary", "first", &peer("a"), 0, 2, vec![1]);
        registry.fulfill_chunk_in(
            "secondary",
            "second",
            &peer("routed-provider"),
            0,
            2,
            vec![1],
        );
        registry.room_reset("secondary");
        assert!(!main.resets.has_changed().unwrap());
        assert_eq!(*first.resets.borrow(), 1);
        assert_eq!(*second.resets.borrow(), 1);
        assert_eq!(registry.state.lock().unwrap().assemblies.len(), 1);
        drop((main, first, second));
        registry.cancel_closed();
        assert!(registry.state.lock().unwrap().assemblies.is_empty());
    }

    #[test]
    fn attempt_records_only_its_paths_and_counts_received_separately_from_accepted() {
        let registry = WantRegistry::new();
        let sender = peer("shared");
        let mut want = registry.register_attempt("cid", sender.clone());
        registry.track_path(want.id, "main");
        registry.fulfill_chunk_in("secondary", "cid", &sender, 0, 2, vec![9]);
        registry.sender_reset(&sender, "secondary");
        assert!(!want.resets.has_changed().unwrap());
        assert_eq!(want.progress.borrow().received, 0);
        registry.fulfill_chunk_in("main", "cid", &sender, 0, 2, vec![1]);
        registry.fulfill_chunk_in("main", "cid", &sender, 0, 2, vec![9]);
        registry.fulfill_chunk_in("main", "cid", &sender, 1, 3, vec![9]);
        assert_eq!(want.progress.borrow().received, 3);
        assert_eq!(want.progress.borrow().accepted, 1);
        assert!(want.progress.borrow().last_accepted.is_some());
        // A fanout WANT uses both paths, so either path can reset that attempt.
        registry.track_path(want.id, "secondary");
        registry.sender_reset(&sender, "secondary");
        assert_eq!(*want.resets.borrow_and_update(), 1);
        registry.fulfill_chunk_in("main", "cid", &sender, 1, 2, vec![2]);
        assert_eq!(want.data.try_recv().unwrap(), vec![1, 2]);
        assert_eq!(want.progress.borrow().received, 4);
        assert_eq!(want.progress.borrow().accepted, 2);
        for i in 0..100 {
            registry.sender_reset(&peer(&i.to_string()), "unused");
        }
        assert!(registry.state.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn query_resets_retain_room_scoped_evidence_without_wants_or_progress() {
        let registry = WantRegistry::new();
        let mut main = registry.query_path_resets("main", &peer("a"));
        let same = registry.query_path_resets("main", &peer("a"));
        let secondary = registry.query_path_resets("secondary", &peer("a"));
        let other = registry.query_path_resets("main", &peer("b"));
        let progress = registry.any_progress();
        registry.sender_reset(&peer("a"), "main");
        registry.sender_reset(&peer("a"), "main");
        assert_eq!(*main.borrow_and_update(), 2);
        assert!(same.has_changed().unwrap());
        assert!(!secondary.has_changed().unwrap());
        assert!(!other.has_changed().unwrap());
        assert_eq!(*progress.borrow(), 0);
        assert!(registry.outstanding_senders().is_empty());
        // A new query after recovery observes only future resets.
        let fresh = registry.query_path_resets("main", &peer("a"));
        assert!(!fresh.has_changed().unwrap());
        registry.room_reset("main");
        assert!(main.has_changed().unwrap());
        assert!(fresh.has_changed().unwrap());
        assert!(other.has_changed().unwrap());
        assert!(!secondary.has_changed().unwrap());
        drop((main, same, secondary, other, fresh));
        registry.cancel_closed();
        assert!(registry.state.lock().unwrap().query_paths.is_empty());
        registry.sender_reset(&peer("unused"), "main");
        assert!(registry.state.lock().unwrap().query_paths.is_empty());
    }

    #[test]
    fn resets_are_sender_scoped_retained_without_progress_and_pruned_with_wants() {
        let registry = WantRegistry::new();
        let first = registry.register_attempt("first", peer("a"));
        let second = registry.register_attempt("second", peer("b"));
        registry.track_path(first.id, "");
        registry.track_path(second.id, "");
        let mut resets = first.resets;
        let other = second.resets;
        let first = first.data;
        let second = second.data;
        let progress = registry.progress(&peer("a"));
        let any_progress = registry.any_progress();
        registry.fulfill_chunk_from("first", &peer("a"), 0, 2, vec![1]);
        registry.fulfill_chunk_from("second", &peer("b"), 0, 2, vec![2]);
        let before = *progress.borrow();
        let any_before = *any_progress.borrow();
        registry.sender_reset(&peer("a"), "");
        registry.sender_reset(&peer("a"), "");
        assert_eq!(*resets.borrow_and_update(), 2);
        assert!(!other.has_changed().unwrap());
        assert_eq!(*progress.borrow(), before);
        assert_eq!(*any_progress.borrow(), any_before);
        let state = registry.state.lock().unwrap();
        assert!(!state
            .assemblies
            .contains_key(&("first".into(), peer("a"), String::new())));
        assert!(state
            .assemblies
            .contains_key(&("second".into(), peer("b"), String::new())));
        drop(state);
        drop((first, second));
        registry.cancel_closed();
        assert!(resets.has_changed().is_err());
        assert!(other.has_changed().is_err());
        for i in 0..100 {
            registry.sender_reset(&peer(&format!("unreferenced-{i}")), "");
        }
        assert!(registry.state.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn outstanding_senders_track_open_registrations_and_prune_closed_assemblies() {
        let registry = WantRegistry::new();
        let first = registry.register_from("first", peer("shared"));
        let second = registry.register_from("second", peer("shared"));
        let completed = registry.register_from("completed", peer("done"));
        registry.fulfill_from("completed", &peer("done"), vec![1]);
        registry.fulfill_chunk_from("first", &peer("shared"), 0, 2, vec![1]);
        assert_eq!(registry.outstanding_senders(), [peer("shared")]);
        drop(first);
        assert_eq!(registry.outstanding_senders(), [peer("shared")]);
        assert!(registry.state.lock().unwrap().assemblies.is_empty());
        drop(second);
        assert!(registry.outstanding_senders().is_empty());
        assert!(registry.state.lock().unwrap().pending.is_empty());
        assert!(registry.state.lock().unwrap().progress.is_empty());
        drop(completed);
    }

    #[tokio::test]
    async fn sender_progress_is_lossless_and_shared_across_cids() {
        let registry = WantRegistry::new();
        let sender = peer("sender");
        let first = registry.register_from("first", sender.clone());
        let second = registry.register_from("second", sender.clone());
        let mut progress = registry.progress(&sender);
        assert_eq!(*progress.borrow(), 0);
        registry.fulfill_chunk_from("first", &sender, 0, 2, vec![1]);
        registry.fulfill_from("second", &sender, vec![2]);
        // Both updates happened before changed() was polled; neither is lost.
        progress.changed().await.unwrap();
        assert_eq!(*progress.borrow_and_update(), 2);
        assert_eq!(second.await.unwrap(), vec![2]);
        registry.fulfill_chunk_from("first", &sender, 1, 2, vec![3]);
        progress.changed().await.unwrap();
        assert_eq!(*progress.borrow(), 3);
        assert_eq!(first.await.unwrap(), vec![1, 3]);
        assert!(registry.state.lock().unwrap().progress.is_empty());
    }

    #[tokio::test]
    async fn registry_progress_is_lossless_across_senders_and_survives_pruning() {
        let registry = WantRegistry::new();
        let mut progress = registry.any_progress();
        let first = registry.register_from("first", peer("one"));
        let second = registry.register_from("second", peer("two"));
        registry.fulfill_chunk_from("first", &peer("one"), 0, 2, vec![1]);
        registry.fulfill_from("second", &peer("two"), vec![2]);
        progress.changed().await.unwrap();
        assert_eq!(*progress.borrow_and_update(), 2);
        assert_eq!(second.await.unwrap(), vec![2]);
        registry.fulfill_chunk_from("first", &peer("one"), 1, 2, vec![3]);
        assert_eq!(first.await.unwrap(), vec![1, 3]);
        registry.cancel_closed();
        progress.changed().await.unwrap();
        assert_eq!(*progress.borrow_and_update(), 3);
        assert!(registry.state.lock().unwrap().progress.is_empty());
        assert!(!progress.has_changed().unwrap());
        let third = registry.register_from("third", peer("three"));
        registry.fulfill_from("third", &peer("three"), vec![4]);
        assert_eq!(third.await.unwrap(), vec![4]);
        progress.changed().await.unwrap();
        assert_eq!(*progress.borrow(), 4);
    }

    #[test]
    fn rejected_and_duplicate_data_do_not_advance_progress() {
        let registry = WantRegistry::new();
        let sender = peer("sender");
        let _want = registry.register_from("cid", sender.clone());
        let progress = registry.progress(&sender);
        let any_progress = registry.any_progress();
        registry.fulfill_chunk_from("cid", &sender, 0, 2, vec![0; MAX_WANT_BLOCK_BYTES]);
        assert_eq!(*progress.borrow(), 1);
        registry.fulfill_chunk_from("cid", &sender, 0, 2, vec![1]); // Duplicate.
        registry.fulfill_chunk_from("cid", &sender, 1, 3, vec![1]); // Contradictory total.
        registry.fulfill_chunk_from("cid", &sender, 1, 2, vec![1]); // Block byte bound.
        registry.fulfill_chunk_from("cid", &sender, 0, 0, vec![1]);
        registry.fulfill_chunk_from("cid", &sender, 0, MAX_WANT_CHUNKS + 1, vec![1]);
        registry.fulfill_chunk_from("cid", &sender, 2, 2, vec![1]);
        registry.fulfill_from("cid", &sender, vec![0; MAX_WANT_BLOCK_BYTES + 1]);
        registry.fulfill_from("unrequested", &sender, vec![1]);
        registry.fulfill_from("cid", &peer("other"), vec![1]);
        assert_eq!(*progress.borrow(), 1);
        assert_eq!(*any_progress.borrow(), 1);
        assert!(!registry
            .state
            .lock()
            .unwrap()
            .progress
            .contains_key(&peer("other")));
    }

    #[test]
    fn progress_entries_are_bounded_by_live_wants_and_reclaimed() {
        let registry = WantRegistry::new();
        let mut wants = Vec::new();
        for i in 0..MAX_PENDING_WANTS {
            let sender = peer(&format!("sender-{i}"));
            wants.push(registry.register_from("cid", sender.clone()));
            let _progress = registry.progress(&sender);
        }
        for i in 0..2 * MAX_WANT_PEERS {
            let sender = peer(&format!("unreferenced-{i}"));
            assert!(registry.progress(&sender).has_changed().is_err());
            registry.fulfill_chunk_from("cid", &sender, 0, 2, vec![1]);
        }
        assert_eq!(
            registry.state.lock().unwrap().progress.len(),
            MAX_PENDING_WANTS
        );
        let rejected = registry.register_from("overflow", peer("overflow"));
        assert!(registry.progress(&peer("overflow")).has_changed().is_err());
        drop(rejected);
        wants.truncate(MAX_PENDING_WANTS / 2);
        registry.cancel_closed();
        assert_eq!(
            registry.state.lock().unwrap().progress.len(),
            MAX_PENDING_WANTS / 2
        );
        drop(wants);
        registry.cancel_closed();
        assert!(registry.state.lock().unwrap().progress.is_empty());

        let _first = registry.register_from("first", peer("shared"));
        let _second = registry.register_from("second", peer("shared"));
        let progress = registry.progress(&peer("shared"));
        registry.cancel("first");
        assert!(progress.has_changed().is_ok());
        registry.cancel("second");
        assert!(progress.has_changed().is_err());
        assert!(registry.state.lock().unwrap().progress.is_empty());
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
            assert!(!registry.state.lock().unwrap().assemblies.contains_key(&(
                cid.clone(),
                peer(""),
                String::new()
            )));
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
    fn chunk_size_caps_large_negotiated_limits_but_honors_smaller_usable_limits() {
        const HEADER_LEN: usize = 6;
        const ENVELOPE_MARGIN: usize = 512;
        let cid = "bafy-short-cid";
        for limit in [65_536, 262_144, u32::MAX] {
            let size = chunk_size_for_limit(cid, Some(limit));
            assert_eq!(size, HAVE_CHUNK_SIZE);
            assert!(size + HEADER_LEN + cid.len() + ENVELOPE_MARGIN <= limit as usize);
        }
        let small = 4096;
        assert_eq!(
            chunk_size_for_limit(cid, Some(small)),
            small as usize - HEADER_LEN - cid.len() - ENVELOPE_MARGIN
        );
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
    fn one_mib_in_64_chunks_reassembles_with_the_existing_wire_layout() {
        let data = vec![42; 1024 * 1024];
        let cid = compute_cid(&data, MULTICODEC_RAW);
        let sender = peer("provider");
        let registry = WantRegistry::new();
        let mut block = registry.register_from(&cid, sender.clone());
        let chunk_size = chunk_size_for_limit(&cid, Some(65_536));
        let total = have_chunk_count(data.len(), chunk_size).unwrap();
        assert_eq!(total, 64);
        assert!(total <= MAX_WANT_CHUNKS);
        // Out-of-order delivery and one duplicate exercise unchanged admission.
        for index in (0..total).rev().chain(std::iter::once(1)) {
            let raw = build_have_payload(&cid, &data, index, total, chunk_size);
            let (parsed_cid, parsed_index, parsed_total, payload) =
                parse_have_chunk_message(&raw).unwrap();
            assert!(payload.len() <= HAVE_CHUNK_SIZE);
            registry.fulfill_chunk_from(&parsed_cid, &sender, parsed_index, parsed_total, payload);
        }
        assert_eq!(block.try_recv().unwrap(), data);
    }

    #[test]
    fn variable_chunk_size_round_trips_across_wire_messages() {
        let cid = compute_cid(b"variable-chunks", MULTICODEC_RAW);
        let chunk_size = 7;
        let data: Vec<u8> = (0..31).collect();
        let total_chunks = have_chunk_count(data.len(), chunk_size).expect("block should fit");
        let mut reassembled = Vec::with_capacity(data.len());

        for chunk_index in 0..total_chunks {
            let message = build_have_payload(&cid, &data, chunk_index, total_chunks, chunk_size);
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
