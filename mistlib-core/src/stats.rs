use crate::types::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStats {
    pub message_count: u64,
    pub send_bits: u64,
    pub receive_bits: u64,
    pub rtt_millis: HashMap<String, f32>,
    pub memory_mb: f32,
    pub eval_send_bits: u64,
    pub eval_receive_bits: u64,
    pub eval_message_count: u64,
    pub nodes: Vec<NodeStats>,
    
    pub diag_peers: usize,
    pub diag_connection_states: usize,
    pub diag_pending_candidates: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStats {
    pub id: String,
    pub sctp_stats: SctpStats,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SctpStats {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub estimated_packet_loss: i64,
    pub state: String,
}

pub struct MistStats {
    pub total_send_bytes: AtomicU64,
    pub total_receive_bytes: AtomicU64,
    pub total_message_count: AtomicU64,
    pub total_eval_send_bytes: AtomicU64,
    pub total_eval_receive_bytes: AtomicU64,
    pub total_eval_message_count: AtomicU64,
    pub rtt_millis: Mutex<Arc<HashMap<NodeId, f32>>>,
}

#[derive(Clone)]
pub struct StatsSnapshot {
    pub message_count: u64,
    pub send_bits: u64,
    pub receive_bits: u64,
    pub rtt_millis: Arc<HashMap<NodeId, f32>>,
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
            rtt_millis: Mutex::new(Arc::new(HashMap::new())),
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
        if let Ok(mut arc) = self.rtt_millis.lock() {
            let map = Arc::make_mut(&mut arc);
            map.insert(node_id, rtt_ms);
        }
    }

    pub fn remove_rtt(&self, node_id: &NodeId) {
        if let Ok(mut arc) = self.rtt_millis.lock() {
            let map = Arc::make_mut(&mut arc);
            map.remove(node_id);
        }
    }

    pub fn remove_node(&self, node_id: &NodeId) {
        if let Ok(mut arc) = self.rtt_millis.lock() {
            let map = Arc::make_mut(&mut arc);
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

        let rtt = {
            let arc = self.rtt_millis.lock().unwrap();
            Arc::clone(&arc)
        };

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_counts() {
        let stats = MistStats::new();
        stats.add_send(100);
        stats.add_send(200);
        stats.add_receive(50);

        let snapshot = stats.snapshot_and_reset();
        assert_eq!(snapshot.message_count, 2);
        assert_eq!(snapshot.send_bits, 300 * 8);
        assert_eq!(snapshot.receive_bits, 50 * 8);

        let snapshot2 = stats.snapshot_and_reset();
        assert_eq!(snapshot2.message_count, 0);
        assert_eq!(snapshot2.send_bits, 0);
    }

    #[test]
    fn test_eval_stats_counts() {
        let stats = MistStats::new();
        stats.add_eval_send(100);
        stats.add_eval_receive(50);

        let snapshot = stats.snapshot_and_reset();
        assert_eq!(snapshot.eval_message_count, 1);
        assert_eq!(snapshot.eval_send_bits, 100 * 8);
        assert_eq!(snapshot.eval_receive_bits, 50 * 8);
    }

    #[test]
    fn test_rtt_stats() {
        let stats = MistStats::new();
        let node_id = NodeId("peer".to_string());
        stats.set_rtt(node_id.clone(), 42.0);

        let snapshot = stats.snapshot_and_reset();
        assert_eq!(snapshot.rtt_millis.get(&node_id), Some(&42.0));

        assert_eq!(stats.rtt_millis.lock().unwrap().get(&node_id), Some(&42.0));

        stats.remove_rtt(&node_id);
        assert_eq!(stats.rtt_millis.lock().unwrap().get(&node_id), None);
    }
}
