use mistlib_core::types::NodeId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub struct MistStats {
    pub total_send_bytes: AtomicU64,
    pub total_receive_bytes: AtomicU64,
    pub total_message_count: AtomicU64,
    pub total_eval_send_bytes: AtomicU64,
    pub total_eval_receive_bytes: AtomicU64,
    pub total_eval_message_count: AtomicU64,
    pub rtt_millis: Mutex<HashMap<NodeId, f32>>,
}

#[derive(Clone)]
pub struct StatsSnapshot {
    pub message_count: u64,
    pub send_bits: u64,
    pub receive_bits: u64,
    pub rtt_millis: HashMap<NodeId, f32>,
    pub eval_send_bits: u64,
    pub eval_receive_bits: u64,
    pub eval_message_count: u64,
}

impl MistStats {
    pub fn new() -> Self {
        Self {
            total_send_bytes: AtomicU64::new(0),
            total_receive_bytes: AtomicU64::new(0),
            total_message_count: AtomicU64::new(0),
            total_eval_send_bytes: AtomicU64::new(0),
            total_eval_receive_bytes: AtomicU64::new(0),
            total_eval_message_count: AtomicU64::new(0),
            rtt_millis: Mutex::new(HashMap::new()),
        }
    }

    pub fn add_send(&self, bytes: u64) {
        self.total_send_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.total_message_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_receive(&self, bytes: u64) {
        self.total_receive_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn add_eval_send(&self, bytes: u64) {
        self.total_eval_send_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        self.total_eval_message_count
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_eval_receive(&self, bytes: u64) {
        self.total_eval_receive_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn set_rtt(&self, node_id: NodeId, rtt_ms: f32) {
        if let Ok(mut map) = self.rtt_millis.lock() {
            map.insert(node_id, rtt_ms);
        }
    }

    pub fn remove_rtt(&self, node_id: &NodeId) {
        if let Ok(mut map) = self.rtt_millis.lock() {
            map.remove(node_id);
        }
    }

    pub fn snapshot_and_reset(&self) -> StatsSnapshot {
        let send_bytes = self.total_send_bytes.swap(0, Ordering::Relaxed);
        let receive_bytes = self.total_receive_bytes.swap(0, Ordering::Relaxed);
        let message_count = self.total_message_count.swap(0, Ordering::Relaxed);
        let eval_send_bytes = self.total_eval_send_bytes.swap(0, Ordering::Relaxed);
        let eval_receive_bytes = self.total_eval_receive_bytes.swap(0, Ordering::Relaxed);
        let eval_message_count = self.total_eval_message_count.swap(0, Ordering::Relaxed);

        let rtt = self
            .rtt_millis
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();

        StatsSnapshot {
            message_count,
            send_bits: send_bytes * 8,
            receive_bits: receive_bytes * 8,
            rtt_millis: rtt,
            eval_send_bits: eval_send_bytes * 8,
            eval_receive_bits: eval_receive_bytes * 8,
            eval_message_count,
        }
    }
}

lazy_static::lazy_static! {
    pub static ref STATS: MistStats = MistStats::new();
}
