use std::collections::HashMap;
use std::fs;

use mistlib_core::stats::{EngineStats, NodeStats, SctpStats};

use super::EngineState;

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

    pub async fn get_stats_json(&self) -> String {
        let snapshot = mistlib_core::stats::STATS.snapshot_and_reset();

        let rtt_millis: HashMap<String, f32> = snapshot
            .rtt_millis
            .iter()
            .map(|(k, v)| (k.0.clone(), *v))
            .collect();

        let t_lock = std::time::Instant::now();
        let wt_ref = {
            let state_lock = self.state.read().await;
            match &*state_lock {
                EngineState::Initialized(ctx) | EngineState::Running(ctx) => {
                    ctx.webrtc_transport.clone()
                }
                _ => None,
            }
        };
        let lock_ms = t_lock.elapsed().as_millis();

        let t_sctp = std::time::Instant::now();
        let (
            mut sctp_raw,
            connection_states,
            diag_peers,
            diag_connection_states,
            diag_pending_candidates,
        ) = if let Some(wt) = wt_ref.as_ref() {
            let stats_future = async {
                let sctp = wt.get_sctp_stats().await;
                let peers = wt.peers.read().await.len();
                let (connection_states, states) = {
                    let states_guard = wt.connection_states.read().unwrap();
                    let connection_states = states_guard
                        .iter()
                        .map(|(node_id, state)| (node_id.0.clone(), state.to_string()))
                        .collect::<HashMap<_, _>>();
                    (connection_states, states_guard.len())
                };
                let cands = wt.pending_candidates.read().await.len();
                (sctp, connection_states, peers, states, cands)
            };
            match tokio::time::timeout(std::time::Duration::from_millis(500), stats_future).await {
                Ok(result) => result,
                Err(_) => {
                    let states_guard = wt.connection_states.read().unwrap();
                    let connection_states = states_guard
                        .iter()
                        .map(|(node_id, state)| (node_id.0.clone(), state.to_string()))
                        .collect::<HashMap<_, _>>();
                    let states = states_guard.len();
                    tracing::error!(
                        "[Diag] get_stats_json TIMEOUT (500ms) lock={}ms conn_states={}",
                        lock_ms,
                        states
                    );
                    (HashMap::new(), connection_states, 0, states, 0)
                }
            }
        } else {
            (HashMap::new(), HashMap::new(), 0, 0, 0)
        };
        let sctp_ms = t_sctp.elapsed().as_millis();

        if lock_ms > 50 || sctp_ms > 100 {
            tracing::error!(
                "[Diag] get_stats_json slow: lock={}ms sctp={}ms peers={} conn_states={}",
                lock_ms,
                sctp_ms,
                diag_peers,
                diag_connection_states
            );
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
                    estimated_packet_loss: 0,
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
