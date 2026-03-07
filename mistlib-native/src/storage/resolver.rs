use async_trait::async_trait;
use mistlib_core::storage::PeerResolver;
use mistlib_core::transport::Transport;
use mistlib_core::types::DeliveryMethod;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

pub const MSG_WANT: u8 = 0x01;
pub const MSG_HAVE: u8 = 0x02;

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Vec<u8>>>>>;

#[derive(Clone)]
pub struct WantRegistry {
    pending: PendingMap,
}

impl WantRegistry {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
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
        let mut want_msg = vec![MSG_WANT];
        want_msg.extend_from_slice(cid.as_bytes());

        let rx = self.registry.register(cid);

        let _ = self
            .transport
            .broadcast(
                bytes::Bytes::from(want_msg),
                DeliveryMethod::ReliableOrdered,
            )
            .await;

        match tokio::time::timeout(std::time::Duration::from_millis(self.timeout_ms), rx).await {
            Ok(Ok(data)) => Some(data),
            _ => {
                self.registry.cancel(cid);
                tracing::debug!("PeerResolver: timeout for CID {}", cid);
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
