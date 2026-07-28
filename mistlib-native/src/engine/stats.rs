use std::collections::HashMap;
use std::fs;

use mistlib_core::stats::{EngineStats, NodeStats, SctpStats};

impl super::MistEngine {
    fn current_memory_mb() -> f32 {
        let Ok(status) = fs::read_to_string("/proc/self/status") else {
            return 0.0;
        };

        status
            .lines()
            .find_map(|line| {
                let rest = line.strip_prefix("VmRSS:")?;
                let kb = rest
                    .split_whitespace()
                    .find_map(|p| p.parse::<u64>().ok())?;
                Some(kb as f32 / 1024.0)
            })
            .unwrap_or(0.0)
    }

    /// Stats stay a process-wide aggregate across all active sessions (v1;
    /// see SPEC-15): per-room SCTP/connection-state maps are merged, so a
    /// NodeId connected in more than one room collapses to a single entry.
    pub async fn get_stats_json(&self) -> String {
        let snapshot = mistlib_core::stats::STATS.snapshot_and_reset();

        let rtt_millis: HashMap<String, f32> = snapshot
            .rtt_millis
            .iter()
            .map(|(k, v)| (k.0.clone(), *v))
            .collect();

        let sessions = self.sessions_snapshot().await;

        let mut sctp_raw = HashMap::new();
        let mut connection_states = HashMap::new();
        let mut diag_peers = 0usize;
        let mut diag_connection_states = 0usize;
        let mut diag_pending_candidates = 0usize;

        for (room_id, ctx) in &sessions {
            let Some(wt) = ctx.webrtc_transport.as_ref() else {
                continue;
            };

            let t_sctp = std::time::Instant::now();
            let stats_future = async {
                let sctp = wt.get_sctp_stats().await;
                let peers = wt.peers.read().await.len();
                let (states_json, states_len) = {
                    let states_guard = wt.connection_states.read().unwrap();
                    let map = states_guard
                        .iter()
                        .map(|(node_id, state)| (node_id.0.clone(), state.to_string()))
                        .collect::<HashMap<_, _>>();
                    (map, states_guard.len())
                };
                let cands = wt.pending_candidates.read().await.len();
                (sctp, states_json, peers, states_len, cands)
            };
            let (sctp, states_json, peers, states_len, cands) =
                match tokio::time::timeout(std::time::Duration::from_millis(500), stats_future)
                    .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        let states_guard = wt.connection_states.read().unwrap();
                        let map = states_guard
                            .iter()
                            .map(|(node_id, state)| (node_id.0.clone(), state.to_string()))
                            .collect::<HashMap<_, _>>();
                        let len = states_guard.len();
                        tracing::error!(
                            "[Diag] get_stats_json TIMEOUT (500ms) room={} conn_states={}",
                            room_id,
                            len
                        );
                        (HashMap::new(), map, 0, len, 0)
                    }
                };
            let sctp_ms = t_sctp.elapsed().as_millis();
            if sctp_ms > 100 {
                tracing::error!(
                    "[Diag] get_stats_json slow: room={} sctp={}ms peers={} conn_states={}",
                    room_id,
                    sctp_ms,
                    peers,
                    states_len
                );
            }

            sctp_raw.extend(sctp);
            connection_states.extend(states_json);
            diag_peers += peers;
            diag_connection_states += states_len;
            diag_pending_candidates += cands;
        }

        let nodes = sctp_raw
            .keys()
            .cloned()
            .chain(connection_states.keys().cloned())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .map(|node_id| {
                let state = connection_states
                    .get(&node_id)
                    .cloned()
                    .or_else(|| sctp_raw.get(&node_id).map(|s| s.state.to_string()))
                    .unwrap_or_else(|| "disconnected".to_string());
                let sctp_stats = sctp_raw.remove(&node_id).map(|s| SctpStats {
                    messages_sent: s.messages_sent,
                    messages_received: s.messages_received,
                    bytes_sent: s.bytes_sent,
                    bytes_received: s.bytes_received,
                    state: s.state.to_string(),
                });
                NodeStats {
                    id: node_id,
                    connection_state: state,
                    sctp_stats,
                }
            })
            .collect();

        let stats = EngineStats {
            message_count: snapshot.message_count,
            send_bits: snapshot.send_bits,
            receive_bits: snapshot.receive_bits,
            rtt_millis,
            memory_mb: Self::current_memory_mb(),
            world_send_bits: snapshot.world_send_bits,
            world_receive_bits: snapshot.world_receive_bits,
            world_message_count: snapshot.world_message_count,
            relay_send_bits: snapshot.relay_send_bits,
            relay_receive_bits: snapshot.relay_receive_bits,
            relay_message_count: snapshot.relay_message_count,
            dropped_receive_events: snapshot.dropped_receive_events,
            dropped_ffi_events: snapshot.dropped_ffi_events,
            nodes,
            diag_peers,
            diag_connection_states,
            diag_pending_candidates,
        };

        match serde_json::to_string(&stats) {
            Ok(json) => json,
            Err(e) => {
                let nan_count = stats
                    .rtt_millis
                    .values()
                    .filter(|v| v.is_nan() || v.is_infinite())
                    .count();
                let mut fixed = stats;
                fixed
                    .rtt_millis
                    .retain(|_, v| !v.is_nan() && !v.is_infinite());
                format!(
                    r#"{{"diagPeers":{},"diagConnectionStates":{},"diagPendingCandidates":{},"diagSerdeError":"{}","diagNanCount":{},"memoryMb":{}}}"#,
                    fixed.diag_peers,
                    fixed.diag_connection_states,
                    fixed.diag_pending_candidates,
                    e,
                    nan_count,
                    fixed.memory_mb
                )
            }
        }
    }
}
