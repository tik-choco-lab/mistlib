use mistlib_core::types::NodeId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub struct MistStats {
    pub total_send_bytes: AtomicU64,
    pub total_receive_bytes: AtomicU64,
    pub total_message_count: AtomicU64,
    pub total_world_send_bytes: AtomicU64,
    pub total_world_receive_bytes: AtomicU64,
    pub total_world_message_count: AtomicU64,
    pub total_relay_send_bytes: AtomicU64,
    pub total_relay_receive_bytes: AtomicU64,
    pub total_relay_message_count: AtomicU64,
    pub rtt_millis: Mutex<HashMap<NodeId, f32>>,
}

#[derive(Clone)]
pub struct StatsSnapshot {
    pub message_count: u64,
    pub send_bits: u64,
    pub receive_bits: u64,
    pub rtt_millis: HashMap<NodeId, f32>,
    pub world_send_bits: u64,
    pub world_receive_bits: u64,
    pub world_message_count: u64,
    pub relay_send_bits: u64,
    pub relay_receive_bits: u64,
    pub relay_message_count: u64,
}

impl Default for MistStats {
    fn default() -> Self {
        Self::new()
    }
}

impl MistStats {
    pub fn new() -> Self {
        Self {
            total_send_bytes: AtomicU64::new(0),
            total_receive_bytes: AtomicU64::new(0),
            total_message_count: AtomicU64::new(0),
            total_world_send_bytes: AtomicU64::new(0),
            total_world_receive_bytes: AtomicU64::new(0),
            total_world_message_count: AtomicU64::new(0),
            total_relay_send_bytes: AtomicU64::new(0),
            total_relay_receive_bytes: AtomicU64::new(0),
            total_relay_message_count: AtomicU64::new(0),
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

    pub fn add_world_send(&self, bytes: u64) {
        self.total_world_send_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        self.total_world_message_count
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_world_receive(&self, bytes: u64) {
        self.total_world_receive_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn add_relay_send(&self, bytes: u64) {
        self.total_relay_send_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        self.total_relay_message_count
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_relay_receive(&self, bytes: u64) {
        self.total_relay_receive_bytes
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
        let world_send_bytes = self.total_world_send_bytes.swap(0, Ordering::Relaxed);
        let world_receive_bytes = self.total_world_receive_bytes.swap(0, Ordering::Relaxed);
        let world_message_count = self.total_world_message_count.swap(0, Ordering::Relaxed);
        let relay_send_bytes = self.total_relay_send_bytes.swap(0, Ordering::Relaxed);
        let relay_receive_bytes = self.total_relay_receive_bytes.swap(0, Ordering::Relaxed);
        let relay_message_count = self.total_relay_message_count.swap(0, Ordering::Relaxed);

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
            world_send_bits: world_send_bytes * 8,
            world_receive_bits: world_receive_bytes * 8,
            world_message_count,
            relay_send_bits: relay_send_bytes * 8,
            relay_receive_bits: relay_receive_bytes * 8,
            relay_message_count,
        }
    }
}

pub static STATS: std::sync::LazyLock<MistStats> = std::sync::LazyLock::new(MistStats::new);
