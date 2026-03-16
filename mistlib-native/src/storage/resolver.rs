use async_trait::async_trait;
use mistlib_core::storage::PeerResolver;
use mistlib_core::transport::Transport;
use mistlib_core::types::DeliveryMethod;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

pub const MSG_WANT: u8 = 0x01;
pub const MSG_HAVE: u8 = 0x02;
pub const MSG_QUERY: u8 = 0x03;
pub const MSG_HAVE_STATUS: u8 = 0x04;

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Vec<u8>>>>>;
type PeerCache = Arc<Mutex<HashMap<String, Vec<mistlib_core::types::NodeId>>>>;
type PeerNotifiers = Arc<Mutex<HashMap<String, Vec<oneshot::Sender<()>>>>>;

#[derive(Clone)]
pub struct WantRegistry {
    pending: PendingMap,
    peer_cache: PeerCache,
    peer_notifiers: PeerNotifiers,
}

impl WantRegistry {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            peer_cache: Arc::new(Mutex::new(HashMap::new())),
            peer_notifiers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register(&self, cid: &str) -> oneshot::Receiver<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(cid.to_string(), tx);
        rx
    }

    pub fn fulfill(&self, cid: &str, data: Vec<u8>) {
        if let Some(tx) = self.pending.lock().unwrap().remove(cid) {
            let _ = tx.send(data);
        }
    }

    pub fn cancel(&self, cid: &str) {
        self.pending.lock().unwrap().remove(cid);
        self.peer_notifiers.lock().unwrap().remove(cid);
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

    pub fn register_peer(&self, cid: &str, peer_id: mistlib_core::types::NodeId) {
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

    pub fn get_peers(&self, cid: &str) -> Vec<mistlib_core::types::NodeId> {
        self.peer_cache
            .lock()
            .unwrap()
            .get(cid)
            .cloned()
            .unwrap_or_default()
    }
}

pub struct NativePeerResolver {
    transport: Arc<dyn Transport>,
    registry: WantRegistry,
    timeout_ms: u64,
}

impl NativePeerResolver {
    pub fn new(transport: Arc<dyn Transport>, registry: WantRegistry, timeout_ms: u64) -> Self {
        Self {
            transport,
            registry,
            timeout_ms,
        }
    }
}

#[async_trait]
impl PeerResolver for NativePeerResolver {
    async fn resolve_block(&self, cid: &str) -> Option<Vec<u8>> {
        let rx_data = self.registry.register(cid);

        
        let mut known_peers = self.registry.get_peers(cid);
        if known_peers.is_empty() {
            tracing::debug!("PeerResolver: Discovery phase for {}", cid);
            let rx_peer = self.registry.register_peer_notifier(cid);
            
            let query_msg = build_query_message(cid);
            let _ = self
                .transport
                .broadcast(
                    bytes::Bytes::from(query_msg),
                    DeliveryMethod::ReliableOrdered,
                )
                .await;

            
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), rx_peer).await;
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
                let _ = self
                    .transport
                    .send(
                        &target,
                        bytes::Bytes::from(want_msg),
                        DeliveryMethod::ReliableOrdered,
                    )
                    .await;
            }
        } else {
            tracing::debug!("PeerResolver: no peers discovered, broadcasting WANT for {}", cid);
            let mut want_msg = vec![MSG_WANT];
            want_msg.extend_from_slice(cid.as_bytes());
            let _ = self
                .transport
                .broadcast(
                    bytes::Bytes::from(want_msg),
                    DeliveryMethod::ReliableOrdered,
                )
                .await;
        }

        
        match tokio::time::timeout(std::time::Duration::from_millis(self.timeout_ms), rx_data).await {
            Ok(Ok(data)) => Some(data),
            _ => {
                self.registry.cancel(cid);
                tracing::debug!("PeerResolver: failed to receive data for CID {}", cid);
                None
            }
        }
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
