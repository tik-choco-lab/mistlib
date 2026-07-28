use crate::overlay::{
    OverlayEnvelope, OVERLAY_MSG_HEARTBEAT, OVERLAY_MSG_NODE_LIST, OVERLAY_MSG_POSITION,
    OVERLAY_MSG_REQUEST_NODE_LIST,
};
use crate::signaling::MessageContent;
use crate::types::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) mod ping;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStats {
    pub message_count: u64,
    pub send_bits: u64,
    pub receive_bits: u64,
    pub rtt_millis: HashMap<String, f32>,
    pub memory_mb: f32,
    pub world_send_bits: u64,
    pub world_receive_bits: u64,
    pub world_message_count: u64,
    pub relay_send_bits: u64,
    pub relay_receive_bits: u64,
    pub relay_message_count: u64,
    pub dropped_receive_events: u64,
    pub dropped_ffi_events: u64,
    pub nodes: Vec<NodeStats>,

    pub diag_peers: usize,
    pub diag_connection_states: usize,
    pub diag_pending_candidates: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStats {
    pub id: String,
    pub connection_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sctp_stats: Option<SctpStats>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SctpStats {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub state: String,
}

/// Per-peer PING/PONG liveness bookkeeping. `awaiting` tracks whether the most
/// recently sent PING for this peer has not yet been answered by the time the
/// next PING is about to go out; `consecutive_misses` counts how many rounds in
/// a row that has happened. A single miss is expected/normal since PING travels
/// Unreliable — this is only meaningful once it crosses a configured threshold.
#[derive(Default)]
struct PingHealth {
    awaiting: bool,
    consecutive_misses: u32,
}

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
    pub dropped_receive_events: AtomicU64,
    pub dropped_ffi_events: AtomicU64,
    pub rtt_millis: Mutex<Arc<HashMap<NodeId, f32>>>,
    ping_health: Mutex<HashMap<NodeId, PingHealth>>,
}

#[derive(Clone)]
pub struct StatsSnapshot {
    pub message_count: u64,
    pub send_bits: u64,
    pub receive_bits: u64,
    pub rtt_millis: Arc<HashMap<NodeId, f32>>,
    pub world_send_bits: u64,
    pub world_receive_bits: u64,
    pub world_message_count: u64,
    pub relay_send_bits: u64,
    pub relay_receive_bits: u64,
    pub relay_message_count: u64,
    pub dropped_receive_events: u64,
    pub dropped_ffi_events: u64,
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
            dropped_receive_events: AtomicU64::new(0),
            dropped_ffi_events: AtomicU64::new(0),
            rtt_millis: Mutex::new(Arc::new(HashMap::new())),
            ping_health: Mutex::new(HashMap::new()),
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

    pub fn add_world_send_frame(&self, data: &[u8]) {
        match classify_overlay_frame(data) {
            Some(OverlayFrameClass::World(bytes)) => self.add_world_send(bytes),
            Some(OverlayFrameClass::Relay(bytes)) => self.add_relay_send(bytes),
            Some(OverlayFrameClass::Excluded) | None => {}
        }
    }

    pub fn add_world_receive_frame(&self, data: &[u8]) {
        match classify_overlay_frame(data) {
            Some(OverlayFrameClass::World(bytes)) => self.add_world_receive(bytes),
            Some(OverlayFrameClass::Relay(bytes)) => self.add_relay_receive(bytes),
            Some(OverlayFrameClass::Excluded) | None => {}
        }
    }

    pub fn add_send_frame(&self, data: &[u8]) {
        match classify_overlay_frame(data) {
            Some(OverlayFrameClass::Excluded) => {}
            class => {
                self.add_send(data.len() as u64);
                match class {
                    Some(OverlayFrameClass::World(bytes)) => self.add_world_send(bytes),
                    Some(OverlayFrameClass::Relay(bytes)) => self.add_relay_send(bytes),
                    Some(OverlayFrameClass::Excluded) | None => {}
                }
            }
        }
    }

    pub fn add_receive_frame(&self, data: &[u8]) {
        match classify_overlay_frame(data) {
            Some(OverlayFrameClass::Excluded) => {}
            class => {
                self.add_receive(data.len() as u64);
                match class {
                    Some(OverlayFrameClass::World(bytes)) => self.add_world_receive(bytes),
                    Some(OverlayFrameClass::Relay(bytes)) => self.add_relay_receive(bytes),
                    Some(OverlayFrameClass::Excluded) | None => {}
                }
            }
        }
    }

    pub fn add_dropped_receive_event(&self) {
        self.dropped_receive_events.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_dropped_ffi_event(&self) {
        self.dropped_ffi_events.fetch_add(1, Ordering::Relaxed);
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
        if let Ok(mut health) = self.ping_health.lock() {
            health.remove(node_id);
        }
    }

    /// Records that a PING is about to be (re)sent to `node_id`. If the previous
    /// round's PING was never answered, bumps the consecutive-miss counter;
    /// otherwise resets it. Returns the updated miss count.
    pub(crate) fn note_ping_sent(&self, node_id: &NodeId) -> u32 {
        let mut health = self.ping_health.lock().unwrap_or_else(|e| e.into_inner());
        let entry = health.entry(node_id.clone()).or_default();
        if entry.awaiting {
            entry.consecutive_misses = entry.consecutive_misses.saturating_add(1);
        } else {
            entry.consecutive_misses = 0;
        }
        entry.awaiting = true;
        entry.consecutive_misses
    }

    /// Records a PONG from `node_id`, clearing its miss streak. `timeout_count`
    /// is the same liveness-suspect threshold `note_ping_sent` is checked
    /// against; returns whether the miss streak had already crossed it (i.e.
    /// whether a `SuspectDisconnected` was latched for this peer), so the
    /// caller knows whether to emit a matching `ClearSuspect`.
    pub(crate) fn note_pong_received(&self, node_id: &NodeId, timeout_count: u32) -> bool {
        let mut health = self.ping_health.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = health.get_mut(node_id) else {
            return false;
        };
        let was_suspect = timeout_count > 0 && entry.consecutive_misses >= timeout_count;
        entry.awaiting = false;
        entry.consecutive_misses = 0;
        was_suspect
    }

    #[cfg(test)]
    pub(crate) fn ping_consecutive_misses(&self, node_id: &NodeId) -> u32 {
        self.ping_health
            .lock()
            .unwrap()
            .get(node_id)
            .map(|e| e.consecutive_misses)
            .unwrap_or(0)
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
        let dropped_receive_events = self.dropped_receive_events.swap(0, Ordering::Relaxed);
        let dropped_ffi_events = self.dropped_ffi_events.swap(0, Ordering::Relaxed);

        let rtt = {
            let arc = self.rtt_millis.lock().unwrap();
            Arc::clone(&arc)
        };

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
            dropped_receive_events,
            dropped_ffi_events,
        }
    }
}

pub static STATS: std::sync::LazyLock<MistStats> = std::sync::LazyLock::new(MistStats::new);

enum OverlayFrameClass {
    World(u64),
    Relay(u64),
    Excluded,
}

pub fn is_network_partition_check_frame(data: &[u8]) -> bool {
    let Ok(envelope) = crate::overlay::wire::deserialize::<OverlayEnvelope>(data) else {
        return false;
    };
    let MessageContent::Raw(payload) = envelope.content else {
        return false;
    };
    is_network_partition_probe_payload(&payload)
}

fn classify_overlay_frame(data: &[u8]) -> Option<OverlayFrameClass> {
    let envelope: OverlayEnvelope = crate::overlay::wire::deserialize(data).ok()?;
    let bytes = data.len() as u64;

    match envelope.content {
        MessageContent::Overlay(msg)
            if matches!(
                msg.message_type,
                OVERLAY_MSG_HEARTBEAT
                    | OVERLAY_MSG_REQUEST_NODE_LIST
                    | OVERLAY_MSG_NODE_LIST
                    | OVERLAY_MSG_POSITION
            ) =>
        {
            Some(OverlayFrameClass::World(bytes))
        }
        MessageContent::Raw(payload) if is_network_partition_probe_payload(&payload) => {
            Some(OverlayFrameClass::Excluded)
        }
        MessageContent::Data(_) | MessageContent::Raw(_) => Some(OverlayFrameClass::Relay(bytes)),
        MessageContent::Overlay(_) => None,
    }
}

fn is_network_partition_probe_payload(payload: &[u8]) -> bool {
    payload
        .windows(br#""type":"network_partition_probe""#.len())
        .any(|window| window == br#""type":"network_partition_probe""#)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{OverlayMessage, OVERLAY_MSG_PING};
    use crate::signaling::{SignalingData, SignalingType};

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
    fn drop_counters_are_reported_and_reset() {
        let stats = MistStats::new();
        stats.add_dropped_receive_event();
        stats.add_dropped_receive_event();
        stats.add_dropped_ffi_event();

        let snapshot = stats.snapshot_and_reset();
        assert_eq!(snapshot.dropped_receive_events, 2);
        assert_eq!(snapshot.dropped_ffi_events, 1);

        let snapshot = stats.snapshot_and_reset();
        assert_eq!(snapshot.dropped_receive_events, 0);
        assert_eq!(snapshot.dropped_ffi_events, 0);
    }

    #[test]
    fn test_eval_stats_counts() {
        let stats = MistStats::new();
        stats.add_world_send(100);
        stats.add_world_receive(50);

        let snapshot = stats.snapshot_and_reset();
        assert_eq!(snapshot.world_message_count, 1);
        assert_eq!(snapshot.world_send_bits, 100 * 8);
        assert_eq!(snapshot.world_receive_bits, 50 * 8);
        assert_eq!(snapshot.relay_message_count, 0);
        assert_eq!(snapshot.relay_send_bits, 0);
        assert_eq!(snapshot.relay_receive_bits, 0);
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

    #[test]
    fn world_frame_stats_count_dnve_control_envelope_bytes() {
        let stats = MistStats::new();
        let envelope = OverlayEnvelope {
            from: NodeId("local".to_string()),
            to: NodeId("peer".to_string()),
            msg_id: 1,
            seq: 0,
            hop_count: 1,
            content: MessageContent::Overlay(OverlayMessage {
                message_type: OVERLAY_MSG_REQUEST_NODE_LIST,
                payload: vec![],
            }),
        };
        let data = crate::overlay::wire::serialize(&envelope).unwrap();

        stats.add_world_send_frame(&data);
        stats.add_world_receive_frame(&data);

        let snapshot = stats.snapshot_and_reset();
        assert_eq!(snapshot.world_message_count, 1);
        assert_eq!(snapshot.world_send_bits, data.len() as u64 * 8);
        assert_eq!(snapshot.world_receive_bits, data.len() as u64 * 8);
        assert_eq!(snapshot.relay_message_count, 0);
        assert_eq!(snapshot.relay_send_bits, 0);
        assert_eq!(snapshot.relay_receive_bits, 0);
    }

    #[test]
    fn world_frame_stats_ignore_ping_and_invalid_frames() {
        let stats = MistStats::new();
        let ping = OverlayEnvelope {
            from: NodeId("local".to_string()),
            to: NodeId("peer".to_string()),
            msg_id: 2,
            seq: 0,
            hop_count: 1,
            content: MessageContent::Overlay(OverlayMessage {
                message_type: OVERLAY_MSG_PING,
                payload: vec![1, 2, 3],
            }),
        };
        let ping_data = crate::overlay::wire::serialize(&ping).unwrap();

        stats.add_world_send_frame(&ping_data);
        stats.add_world_receive_frame(b"not-an-envelope");

        let snapshot = stats.snapshot_and_reset();
        assert_eq!(snapshot.world_message_count, 0);
        assert_eq!(snapshot.world_send_bits, 0);
        assert_eq!(snapshot.world_receive_bits, 0);
    }

    #[test]
    fn frame_stats_count_relay_signaling_envelope_bytes_separately() {
        let stats = MistStats::new();
        let envelope = OverlayEnvelope {
            from: NodeId("local".to_string()),
            to: NodeId("peer".to_string()),
            msg_id: 1,
            seq: 0,
            hop_count: 1,
            content: MessageContent::Data(SignalingData {
                sender_id: NodeId("local".to_string()),
                receiver_id: NodeId("peer".to_string()),
                room_id: "room".to_string(),
                data: "candidate".to_string(),
                signaling_type: SignalingType::Candidate,
            }),
        };
        let data = crate::overlay::wire::serialize(&envelope).unwrap();

        stats.add_world_send_frame(&data);
        stats.add_world_receive_frame(&data);

        let snapshot = stats.snapshot_and_reset();
        assert_eq!(snapshot.world_message_count, 0);
        assert_eq!(snapshot.world_send_bits, 0);
        assert_eq!(snapshot.world_receive_bits, 0);
        assert_eq!(snapshot.relay_message_count, 1);
        assert_eq!(snapshot.relay_send_bits, data.len() as u64 * 8);
        assert_eq!(snapshot.relay_receive_bits, data.len() as u64 * 8);
    }

    #[test]
    fn frame_stats_count_relay_raw_envelope_bytes_separately() {
        let stats = MistStats::new();
        let envelope = OverlayEnvelope {
            from: NodeId("local".to_string()),
            to: NodeId("peer".to_string()),
            msg_id: 1,
            seq: 0,
            hop_count: 1,
            content: MessageContent::Raw(bytes::Bytes::from_static(b"world payload")),
        };
        let data = crate::overlay::wire::serialize(&envelope).unwrap();

        stats.add_world_send_frame(&data);

        let snapshot = stats.snapshot_and_reset();
        assert_eq!(snapshot.world_message_count, 0);
        assert_eq!(snapshot.world_send_bits, 0);
        assert_eq!(snapshot.relay_message_count, 1);
        assert_eq!(snapshot.relay_send_bits, data.len() as u64 * 8);
    }

    #[test]
    fn frame_stats_ignore_network_partition_probe() {
        let stats = MistStats::new();
        let envelope = OverlayEnvelope {
            from: NodeId("local".to_string()),
            to: NodeId::broadcast(),
            msg_id: 3,
            seq: 0,
            hop_count: 3,
            content: MessageContent::Raw(bytes::Bytes::from_static(
                br#"{"type":"network_partition_probe","id":"probe-1","origin":"local"}"#,
            )),
        };
        let data = crate::overlay::wire::serialize(&envelope).unwrap();

        assert!(is_network_partition_check_frame(&data));
        stats.add_send_frame(&data);
        stats.add_receive_frame(&data);

        let snapshot = stats.snapshot_and_reset();
        assert_eq!(snapshot.message_count, 0);
        assert_eq!(snapshot.send_bits, 0);
        assert_eq!(snapshot.receive_bits, 0);
        assert_eq!(snapshot.world_message_count, 0);
        assert_eq!(snapshot.world_send_bits, 0);
        assert_eq!(snapshot.world_receive_bits, 0);
        assert_eq!(snapshot.relay_message_count, 0);
        assert_eq!(snapshot.relay_send_bits, 0);
        assert_eq!(snapshot.relay_receive_bits, 0);
    }
}
