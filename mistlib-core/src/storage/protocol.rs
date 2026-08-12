use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

pub const MSG_WANT: u8 = 0x01;
pub const MSG_HAVE: u8 = 0x02;
pub const MSG_QUERY: u8 = 0x03;
pub const MSG_HAVE_STATUS: u8 = 0x04;
pub const MSG_HAVE_CHUNK: u8 = 0x05;
pub const HAVE_CHUNK_SIZE: usize = 16 * 1024;

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Vec<u8>>>>>;
type PeerCache = Arc<Mutex<HashMap<String, Vec<crate::types::NodeId>>>>;
type PeerNotifiers = Arc<Mutex<HashMap<String, Vec<oneshot::Sender<()>>>>>;
type ChunkMap = Arc<Mutex<HashMap<String, ChunkAssembly>>>;

#[derive(Debug, Clone)]
struct ChunkAssembly {
    total_chunks: u16,
    received_chunks: u16,
    chunks: Vec<Option<Vec<u8>>>,
}

#[derive(Clone)]
pub struct WantRegistry {
    pending: PendingMap,
    peer_cache: PeerCache,
    peer_notifiers: PeerNotifiers,
    chunk_assemblies: ChunkMap,
}

impl Default for WantRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WantRegistry {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            peer_cache: Arc::new(Mutex::new(HashMap::new())),
            peer_notifiers: Arc::new(Mutex::new(HashMap::new())),
            chunk_assemblies: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register(&self, cid: &str) -> oneshot::Receiver<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(cid.to_string(), tx);
        rx
    }

    pub fn fulfill(&self, cid: &str, data: Vec<u8>) {
        self.chunk_assemblies.lock().unwrap().remove(cid);
        if let Some(tx) = self.pending.lock().unwrap().remove(cid) {
            let _ = tx.send(data);
        }
    }

    pub fn fulfill_chunk(&self, cid: &str, chunk_index: u16, chunk_total: u16, data: Vec<u8>) {
        if chunk_total == 0 || chunk_index >= chunk_total {
            return;
        }

        let mut assembled_payload: Option<Vec<u8>> = None;

        {
            let mut assemblies = self.chunk_assemblies.lock().unwrap();
            let entry = assemblies
                .entry(cid.to_string())
                .or_insert_with(|| ChunkAssembly {
                    total_chunks: chunk_total,
                    received_chunks: 0,
                    chunks: vec![None; chunk_total as usize],
                });

            if entry.total_chunks != chunk_total {
                *entry = ChunkAssembly {
                    total_chunks: chunk_total,
                    received_chunks: 0,
                    chunks: vec![None; chunk_total as usize],
                };
            }

            let slot = &mut entry.chunks[chunk_index as usize];
            if slot.is_none() {
                *slot = Some(data);
                entry.received_chunks += 1;
            }

            if entry.received_chunks == entry.total_chunks {
                let mut full = Vec::new();
                for chunk in &mut entry.chunks {
                    if let Some(part) = chunk.take() {
                        full.extend_from_slice(&part);
                    }
                }
                assembled_payload = Some(full);
                assemblies.remove(cid);
            }
        }

        if let Some(full) = assembled_payload {
            self.fulfill(cid, full);
        }
    }

    pub fn cancel(&self, cid: &str) {
        self.pending.lock().unwrap().remove(cid);
        self.peer_notifiers.lock().unwrap().remove(cid);
        self.chunk_assemblies.lock().unwrap().remove(cid);
    }

    pub fn register_peer_notifier(&self, cid: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.peer_notifiers
            .lock()
            .unwrap()
            .entry(cid.to_string())
            .or_default()
            .push(tx);
        rx
    }

    pub fn register_peer(&self, cid: &str, peer_id: crate::types::NodeId) {
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

    pub fn get_peers(&self, cid: &str) -> Vec<crate::types::NodeId> {
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
}
