use std::collections::HashMap;
use std::env;

use mistlib_core::types::ConnectionState;
use webrtc::stats::StatsReportType;

use super::{Peer, WebRtcTransport};

/// webrtc-rs 0.13 exposes ICE counters only through PC.get_stats(): its
/// ICE-only collectors and agent getter are private. The public collector also
/// visits DataChannels and may wait on a pending send. Use only Transport stats,
/// bound the whole call, and sample peers concurrently at the sweeper cadence.
/// An upstream ICE-only getter would remove this residual send-lock contention.
pub(super) async fn transport_received_bytes(peer: &Peer) -> Option<(String, u64)> {
    let report = tokio::time::timeout(std::time::Duration::from_millis(250), peer.pc.get_stats())
        .await
        .ok()?;
    report.reports.into_values().find_map(|stat| match stat {
        StatsReportType::Transport(s) => Some((s.id, s.bytes_received as u64)),
        _ => None,
    })
}

#[derive(Debug, Clone)]
pub struct SctpPeerStats {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub state: ConnectionState,
}

static SCTP_STATS_ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

impl WebRtcTransport {
    pub async fn get_sctp_stats(&self) -> HashMap<String, SctpPeerStats> {
        // Collected by default. Disable by setting MISTLIB_DISABLE_SCTP_STATS=1.
        let enabled = *SCTP_STATS_ENABLED.get_or_init(|| {
            !matches!(
                env::var("MISTLIB_DISABLE_SCTP_STATS").as_deref(),
                Ok("1" | "true" | "True" | "TRUE")
            )
        });
        if !enabled {
            return HashMap::new();
        }

        let peers_list = {
            let peers = self.peers.read().await;
            peers
                .iter()
                .map(|(id, p)| (id.clone(), p.clone()))
                .collect::<Vec<_>>()
        };

        let mut result = HashMap::new();

        for (node_id, peer) in peers_list {
            let state = {
                let states = self.connection_states.read().unwrap();
                states
                    .get(&node_id)
                    .copied()
                    .unwrap_or(ConnectionState::Disconnected)
            };

            if state == ConnectionState::Disconnected || state == ConnectionState::Failed {
                result.insert(
                    node_id.0.clone(),
                    SctpPeerStats {
                        messages_sent: 0,
                        messages_received: 0,
                        bytes_sent: 0,
                        bytes_received: 0,
                        state,
                    },
                );
                continue;
            }

            let report = match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                peer.pc.get_stats(),
            )
            .await
            {
                Ok(report) => report,
                Err(_) => {
                    result.insert(
                        node_id.0.clone(),
                        SctpPeerStats {
                            messages_sent: 0,
                            messages_received: 0,
                            bytes_sent: 0,
                            bytes_received: 0,
                            state,
                        },
                    );
                    continue;
                }
            };

            let mut peer_stats = SctpPeerStats {
                messages_sent: 0,
                messages_received: 0,
                bytes_sent: 0,
                bytes_received: 0,
                state,
            };

            for (_, stat) in report.reports {
                if let StatsReportType::DataChannel(s) = stat {
                    peer_stats.messages_sent += s.messages_sent as u64;
                    peer_stats.messages_received += s.messages_received as u64;
                    peer_stats.bytes_sent += s.bytes_sent as u64;
                    peer_stats.bytes_received += s.bytes_received as u64;
                }
            }

            result.insert(node_id.0.clone(), peer_stats);
        }

        result
    }
}

impl WebRtcTransport {
    /// Use the pacing observer's cached buffer sample: webrtc-rs holds its DC
    /// mutex over blocked sends, so stats must not issue another buffer query.
    pub async fn storage_transport_stats(&self) -> HashMap<String, serde_json::Value> {
        let peers: Vec<_> = self
            .peers
            .read()
            .await
            .iter()
            .map(|(id, peer)| (id.clone(), peer.send_tx.clone(), peer.pacing.clone()))
            .collect();
        let mut telemetry = self.queue_telemetry.lock().unwrap();
        telemetry.retain(|id, _| peers.iter().any(|(current, _, _)| current == id));
        let budgets = self.bulk_budgets.lock().unwrap();
        peers
            .into_iter()
            .map(|(id, tx, pacing)| {
                let counters = telemetry.get(&id);
                let budget = budgets.get(&id).and_then(|generations| {
                    generations.iter().find_map(|(sender, budget)| {
                        sender
                            .upgrade()
                            .filter(|old| old.same_channel(&tx))
                            .map(|_| budget.limit())
                    })
                });
                let (buffered, rate) = pacing.as_ref().map_or((None, None), |p| p.snapshot());
                (
                    id.0,
                    serde_json::json!({
                        "bulkQueuedBytes": counters.map_or(0, |c| c.queued_bytes()),
                        "effectiveBulkBudgetBytes": budget,
                        "bulkChannel": counters.and_then(|c| c.bulk_channel()),
                        "pacingBufferedAmount": buffered,
                        "pacingDeliveryBytesPerSecond": rate,
                    }),
                )
            })
            .collect()
    }
}
