use std::collections::HashMap;
use std::env;

use mistlib_core::types::ConnectionState;
use webrtc::stats::StatsReportType;

use super::WebRtcTransport;

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
        // Enable by setting MISTLIB_ENABLE_SCTP_STATS=1.
        let enabled = *SCTP_STATS_ENABLED.get_or_init(|| {
            matches!(
                env::var("MISTLIB_ENABLE_SCTP_STATS").as_deref(),
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
