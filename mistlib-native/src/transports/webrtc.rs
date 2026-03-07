use async_trait::async_trait;
use bytes::Bytes;
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::stats::STATS;
use mistlib_core::transport::{NetworkEvent, NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock as StdRwLock};

use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::api::API;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::stats::StatsReportType;

const SERVER_ID: &str = "server";

#[derive(Debug, Clone)]
pub struct SctpPeerStats {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub state: ConnectionState,
}

pub mod peer;
pub mod signaling;

pub use peer::Peer;

pub struct WebRtcTransport {
    pub signaler: Arc<dyn Signaler>,
    pub local_node_id: NodeId,
    pub api: API,
    pub peers: Arc<tokio::sync::RwLock<HashMap<NodeId, Arc<Peer>>>>,
    pub event_handler: Mutex<Option<Arc<dyn NetworkEventHandler>>>,
    pub connection_states: Arc<StdRwLock<HashMap<NodeId, ConnectionState>>>,
    pub room_id: Arc<StdRwLock<String>>,
    pub pending_candidates: Arc<tokio::sync::RwLock<HashMap<NodeId, Vec<String>>>>,
    pub max_connections: AtomicU32,
}

impl WebRtcTransport {
    pub fn new(signaler: Arc<dyn Signaler>, local_node_id: NodeId) -> Self {
        let m = MediaEngine::default();
        let api = APIBuilder::new().with_media_engine(m).build();

        Self {
            signaler,
            local_node_id,
            api,
            peers: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            event_handler: Mutex::new(None),
            connection_states: Arc::new(StdRwLock::new(HashMap::new())),
            room_id: Arc::new(StdRwLock::new("lobby".to_string())),
            pending_candidates: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            max_connections: AtomicU32::new(30),
        }
    }

    pub fn set_room_id(&self, room_id: String) {
        let mut room = self.room_id.write().unwrap();
        *room = room_id;
    }

    pub fn set_max_connections(&self, max: u32) {
        self.max_connections.store(max, Ordering::Relaxed);
    }

    fn get_room_id(&self) -> String {
        self.room_id.read().unwrap().clone()
    }

    async fn create_pc(&self, remote_id: NodeId) -> crate::error::Result<Arc<Peer>> {
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_string()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let cancel_token = tokio_util::sync::CancellationToken::new();
        let pc = Arc::new(self.api.new_peer_connection(config).await?);
        let peer = Arc::new(Peer {
            pc: pc.clone(),
            channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            cancel_token: cancel_token.clone(),
        });

        let handler_opt = {
            let h = self.event_handler.lock().unwrap();
            h.clone()
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<NetworkEvent>(2048);
        if let Some(h) = handler_opt {
            let h_clone = h.clone();
            let ct = cancel_token.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = ct.cancelled() => break,
                        Some(event) = rx.recv() => {
                            h_clone.on_event(event);
                        }
                        else => break,
                    }
                }
            });
        }

        peer.setup_handlers(
            remote_id,
            self.signaler.clone(),
            self.local_node_id.clone(),
            self.get_room_id(),
            Some(tx),
            self.connection_states.clone(),
            self.peers.clone(),
            self.pending_candidates.clone(),
        )
        .await?;

        Ok(peer)
    }

    pub async fn get_sctp_stats(&self) -> HashMap<String, SctpPeerStats> {
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
                match stat {
                    StatsReportType::DataChannel(s) => {
                        peer_stats.messages_sent += s.messages_sent as u64;
                        peer_stats.messages_received += s.messages_received as u64;
                        peer_stats.bytes_sent += s.bytes_sent as u64;
                        peer_stats.bytes_received += s.bytes_received as u64;
                    }
                    _ => {}
                }
            }

            result.insert(node_id.0.clone(), peer_stats);
        }

        result
    }

    async fn connect_inner(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        let peer = self
            .create_pc(node.clone())
            .await
            .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;

        let methods = vec![
            (DeliveryMethod::ReliableOrdered, "reliable", None),
            (
                DeliveryMethod::UnreliableOrdered,
                "unreliable-ordered",
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    max_retransmits: Some(0),
                    ..Default::default()
                }),
            ),
            (
                DeliveryMethod::Unreliable,
                "unreliable",
                Some(RTCDataChannelInit {
                    ordered: Some(false),
                    max_retransmits: Some(0),
                    ..Default::default()
                }),
            ),
        ];

        for (method, label, init) in methods {
            let dc = peer
                .pc
                .create_data_channel(label, init)
                .await
                .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;

            let handler_opt = {
                let h = self.event_handler.lock().unwrap();
                h.clone()
            };

            if let Some(h) = handler_opt {
                let (tx, mut rx) = tokio::sync::mpsc::channel::<NetworkEvent>(2048);
                let h_clone = h.clone();
                let ct = peer.cancel_token.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = ct.cancelled() => break,
                            Some(event) = rx.recv() => {
                                h_clone.on_event(event);
                            }
                            else => break,
                        }
                    }
                });
                Peer::setup_dc_handlers(
                    dc.clone(),
                    Some(tx),
                    node.clone(),
                    peer.cancel_token.clone(),
                )
                .await;
            } else {
                Peer::setup_dc_handlers(dc.clone(), None, node.clone(), peer.cancel_token.clone())
                    .await;
            }

            {
                let mut dc_lock = peer.channels.write().await;
                dc_lock.insert(method, dc);
            }
        }

        {
            let mut peers = self.peers.write().await;
            peers.insert(node.clone(), peer.clone());
        }

        let result: mistlib_core::error::Result<()> = async {
            let offer = peer
                .pc
                .create_offer(None)
                .await
                .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;
            peer.pc
                .set_local_description(offer)
                .await
                .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;

            if let Some(offer_desc) = peer.pc.local_description().await {
                let data = serde_json::to_string(&offer_desc)
                    .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;
                self.signaler
                    .send_signaling(
                        node,
                        MessageContent::Data(SignalingData {
                            sender_id: self.local_node_id.clone(),
                            receiver_id: node.clone(),
                            room_id: self.get_room_id(),
                            data,
                            signaling_type: SignalingType::Offer,
                        }),
                    )
                    .await?;
            }
            Ok(())
        }
        .await;

        if result.is_err() {
            let removed_peer = {
                let mut peers = self.peers.write().await;
                peers.remove(node)
            };
            if let Some(p) = removed_peer {
                p.close_all().await;
            }
        }

        result
    }
}

#[async_trait]
impl Transport for WebRtcTransport {
    async fn start(
        &self,
        handler: Arc<dyn NetworkEventHandler>,
    ) -> mistlib_core::error::Result<()> {
        {
            let mut h = self.event_handler.lock().unwrap();
            *h = Some(handler);
        }

        let room_id = self.get_room_id();

        self.signaler
            .send_signaling(
                &NodeId(SERVER_ID.to_string()),
                MessageContent::Data(SignalingData {
                    sender_id: self.local_node_id.clone(),
                    receiver_id: NodeId("".to_string()),
                    room_id,
                    data: "".to_string(),
                    signaling_type: SignalingType::Request,
                }),
            )
            .await?;
        Ok(())
    }

    async fn send(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> mistlib_core::error::Result<()> {
        let peer = {
            let peers = self.peers.read().await;
            peers.get(node).cloned()
        }
        .ok_or_else(|| {
            mistlib_core::error::MistError::Internal(format!("Node not found: {:?}", node))
        })?;

        let dc = {
            let dc_lock = peer.channels.read().await;
            dc_lock.get(&method).cloned()
        }
        .ok_or_else(|| {
            mistlib_core::error::MistError::Internal(format!("No DC for method {:?}", method))
        })?;

        if dc.ready_state() == RTCDataChannelState::Open {
            dc.send(&data)
                .await
                .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;
            STATS.add_send(data.len() as u64);
            return Ok(());
        }
        Err(mistlib_core::error::MistError::Internal(format!(
            "Channel for {:?} is not open (state: {:?})",
            method,
            dc.ready_state()
        )))
    }

    async fn broadcast(
        &self,
        data: Bytes,
        method: DeliveryMethod,
    ) -> mistlib_core::error::Result<()> {
        let targets = self.get_connected_nodes();
        for target in targets {
            let _ = self.send(&target, data.clone(), method).await;
        }
        Ok(())
    }

    fn get_connection_state(&self, node: &NodeId) -> ConnectionState {
        let states = self.connection_states.read().unwrap();
        states
            .get(node)
            .cloned()
            .unwrap_or(ConnectionState::Disconnected)
    }

    async fn connect(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        {
            let peers = self.peers.read().await;
            if peers.contains_key(node) {
                return Ok(());
            }
        }

        {
            let mut states = self.connection_states.write().unwrap();
            if states.contains_key(node) {
                return Ok(());
            }
            let max = self.max_connections.load(Ordering::Relaxed) as usize;
            let count = states
                .values()
                .filter(|s| **s == ConnectionState::Connected || **s == ConnectionState::Connecting)
                .count();
            if count >= max {
                return Ok(());
            }
            states.insert(node.clone(), ConnectionState::Connecting);
            tracing::error!("[CS] INSERT connect: {} total={}", node, states.len());
        }

        let result = self.connect_inner(node).await;
        if result.is_err() {
            let mut states = self.connection_states.write().unwrap();
            states.remove(node);
            tracing::error!("[CS] REMOVE connect_err: {} total={}", node, states.len());
        }
        result
    }

    async fn disconnect(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        {
            let mut states = self.connection_states.write().unwrap();
            states.remove(node);
            tracing::error!("[CS] REMOVE disconnect: {} total={}", node, states.len());
        }

        let peer = {
            let mut peers = self.peers.write().await;
            peers.remove(node)
        };

        {
            let mut pc_lock = self.pending_candidates.write().await;
            pc_lock.remove(node);
        }

        if let Some(peer) = peer {
            peer.close_all().await;
        }

        crate::events::on_disconnected_internal(node.clone());
        Ok(())
    }

    fn get_connected_nodes(&self) -> Vec<NodeId> {
        let states = self.connection_states.read().unwrap();
        states
            .iter()
            .filter(|(_, &s)| s == ConnectionState::Connected)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

impl WebRtcTransport {
    pub fn get_active_connection_states(&self) -> Vec<(NodeId, ConnectionState)> {
        let states = self.connection_states.read().unwrap();
        states
            .iter()
            .filter(|(_, s)| {
                **s == ConnectionState::Connected || **s == ConnectionState::Connecting
            })
            .map(|(id, s)| (id.clone(), *s))
            .collect()
    }
}

#[cfg(test)]
mod tests;
