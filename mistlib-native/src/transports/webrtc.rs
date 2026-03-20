use async_trait::async_trait;
use bytes::Bytes;
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::stats::STATS;
use mistlib_core::transport::{NetworkEvent, NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::env;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::api::API;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::signaling_state::RTCSignalingState;
use webrtc::stats::StatsReportType;

const SERVER_ID: &str = "server";
const CONNECTION_TIMEOUT_MS: u64 = 6000;
const RECONNECT_COOLDOWN_MS: u64 = 3000;

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
    pub connection_attempt_ids: Arc<StdRwLock<HashMap<NodeId, u32>>>,
    pub last_disconnect_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    pub next_connection_attempt_id: AtomicU32,
    pub sweeper_started: AtomicBool,
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
            connection_attempt_ids: Arc::new(StdRwLock::new(HashMap::new())),
            last_disconnect_at: Arc::new(StdRwLock::new(HashMap::new())),
            next_connection_attempt_id: AtomicU32::new(1),
            sweeper_started: AtomicBool::new(false),
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
            self.connection_attempt_ids.clone(),
            self.last_disconnect_at.clone(),
        )
        .await?;

        Ok(peer)
    }

    fn reserve_connection_attempt(&self, node: &NodeId) -> u32 {
        let attempt_id = self
            .next_connection_attempt_id
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let mut attempts = self.connection_attempt_ids.write().unwrap();
        attempts.insert(node.clone(), attempt_id);
        attempt_id
    }

    async fn cleanup_session(&self, node: &NodeId, force_failed: bool) {
        {
            let mut attempts = self.connection_attempt_ids.write().unwrap();
            attempts.remove(node);
        }

        {
            let mut last_disconnect = self.last_disconnect_at.write().unwrap();
            last_disconnect.insert(node.clone(), Instant::now());
        }

        {
            let mut states = self.connection_states.write().unwrap();
            if force_failed {
                states.insert(node.clone(), ConnectionState::Failed);
            } else {
                states.remove(node);
            }
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
    }

    fn can_send_offer(
        &self,
        node: &NodeId,
        pc_state: RTCPeerConnectionState,
        signaling_state: RTCSignalingState,
    ) -> bool {
        let state_ok = {
            let states = self.connection_states.read().unwrap();
            matches!(
                states.get(node),
                Some(ConnectionState::Connecting) | Some(ConnectionState::Connected)
            )
        };

        if !state_ok {
            tracing::warn!(
                "[Signaling] Reject Offer to {}: invalid connection state",
                node
            );
            return false;
        }

        if signaling_state != RTCSignalingState::Stable {
            tracing::warn!(
                "[Signaling] Reject Offer to {}: signaling is not stable ({:?})",
                node,
                signaling_state
            );
            return false;
        }

        if matches!(
            pc_state,
            RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed
                | RTCPeerConnectionState::Disconnected
        ) {
            tracing::warn!(
                "[Signaling] Reject Offer to {}: pc state is unstable ({:?})",
                node,
                pc_state
            );
            return false;
        }

        true
    }

    fn spawn_connection_watchdog(&self, node: NodeId, attempt_id: u32) {
        let peers = self.peers.clone();
        let states = self.connection_states.clone();
        let pending = self.pending_candidates.clone();
        let attempts = self.connection_attempt_ids.clone();

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(CONNECTION_TIMEOUT_MS)).await;

            let is_current_attempt = {
                let lock = attempts.read().unwrap();
                matches!(lock.get(&node), Some(id) if *id == attempt_id)
            };

            if !is_current_attempt {
                return;
            }

            let still_connecting = {
                let lock = states.read().unwrap();
                matches!(lock.get(&node), Some(ConnectionState::Connecting))
            };

            let has_open_channel = {
                let peer_opt = {
                    let lock = peers.read().await;
                    lock.get(&node).cloned()
                };
                if let Some(peer) = peer_opt {
                    let channels = peer.channels.read().await;
                    channels
                        .values()
                        .any(|dc| dc.ready_state() == RTCDataChannelState::Open)
                } else {
                    false
                }
            };

            if still_connecting && !has_open_channel {
                {
                    let mut lock = attempts.write().unwrap();
                    lock.remove(&node);
                }
                {
                    let mut lock = states.write().unwrap();
                    lock.insert(node.clone(), ConnectionState::Failed);
                }
                let peer = {
                    let mut lock = peers.write().await;
                    lock.remove(&node)
                };
                {
                    let mut lock = pending.write().await;
                    lock.remove(&node);
                }
                if let Some(peer) = peer {
                    peer.close_all().await;
                }
                crate::events::on_disconnected_internal(node.clone());
                tracing::warn!(
                    "[Watchdog] Session cleanup on timeout for {} (attempt={})",
                    node,
                    attempt_id
                );
            }
        });
    }

    fn ensure_session_sweeper(&self) {
        if self.sweeper_started.swap(true, Ordering::SeqCst) {
            return;
        }

        let peers = self.peers.clone();
        let states = self.connection_states.clone();
        let pending = self.pending_candidates.clone();
        let attempts = self.connection_attempt_ids.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

                let nodes = {
                    let lock = states.read().unwrap();
                    lock.keys().cloned().collect::<Vec<_>>()
                };

                for node in nodes {
                    let peer_opt = {
                        let lock = peers.read().await;
                        lock.get(&node).cloned()
                    };

                    let Some(peer) = peer_opt else {
                        let mut lock = states.write().unwrap();
                        lock.remove(&node);
                        continue;
                    };

                    let pc_state = peer.pc.connection_state();
                    let state_snapshot = {
                        let lock = states.read().unwrap();
                        lock.get(&node).copied().unwrap_or(ConnectionState::Disconnected)
                    };
                    let has_open_channel = {
                        let channels = peer.channels.read().await;
                        channels
                            .values()
                            .any(|dc| dc.ready_state() == RTCDataChannelState::Open)
                    };

                    let should_cleanup = matches!(
                        pc_state,
                        RTCPeerConnectionState::Failed
                            | RTCPeerConnectionState::Closed
                            | RTCPeerConnectionState::Disconnected
                    ) || (state_snapshot == ConnectionState::Connected && !has_open_channel);

                    if should_cleanup {
                        {
                            let mut lock = attempts.write().unwrap();
                            lock.remove(&node);
                        }
                        {
                            let mut lock = states.write().unwrap();
                            lock.insert(node.clone(), ConnectionState::Failed);
                        }
                        {
                            let mut lock = peers.write().await;
                            lock.remove(&node);
                        }
                        {
                            let mut lock = pending.write().await;
                            lock.remove(&node);
                        }
                        peer.close_all().await;
                        crate::events::on_disconnected_internal(node.clone());
                        tracing::warn!("[Sweeper] Force cleaned session for {}", node);
                    }
                }
            }
        });
    }

    pub async fn get_sctp_stats(&self) -> HashMap<String, SctpPeerStats> {
        
        
        
        let enabled = match env::var("MISTLIB_ENABLE_SCTP_STATS") {
            Ok(val) => matches!(val.as_str(), "1" | "true" | "True" | "TRUE"),
            Err(_) => false,
        };
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

        let attempt_id = self.reserve_connection_attempt(node);
        self.spawn_connection_watchdog(node.clone(), attempt_id);

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
                    self.connection_states.clone(),
                    self.peers.clone(),
                    self.pending_candidates.clone(),
                    self.connection_attempt_ids.clone(),
                    self.last_disconnect_at.clone(),
                )
                .await;
            } else {
                Peer::setup_dc_handlers(
                    dc.clone(),
                    None,
                    node.clone(),
                    peer.cancel_token.clone(),
                    self.connection_states.clone(),
                    self.peers.clone(),
                    self.pending_candidates.clone(),
                    self.connection_attempt_ids.clone(),
                    self.last_disconnect_at.clone(),
                )
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
            let signaling_state = peer.pc.signaling_state();
            let pc_state = peer.pc.connection_state();
            if !self.can_send_offer(node, pc_state, signaling_state) {
                return Err(mistlib_core::error::MistError::Internal(
                    "Offer precondition failed".to_string(),
                ));
            }

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
            self.cleanup_session(node, true).await;
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
        self.ensure_session_sweeper();

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

        let wait_duration = {
            let last_disconnect = self.last_disconnect_at.read().unwrap();
            last_disconnect.get(node).copied().and_then(|at| {
                let elapsed = at.elapsed();
                if elapsed < Duration::from_millis(RECONNECT_COOLDOWN_MS) {
                    Some(Duration::from_millis(RECONNECT_COOLDOWN_MS) - elapsed)
                } else {
                    None
                }
            })
        };

        if let Some(wait_duration) = wait_duration {
            tracing::warn!(
                "[Reconnect] waiting {:?} before retrying connection to {}",
                wait_duration,
                node
            );
            tokio::time::sleep(wait_duration).await;
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
            let states = self.connection_states.read().unwrap();
            tracing::error!("[CS] FAILED connect_err: {} total={}", node, states.len());
        }
        result
    }

    async fn disconnect(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        self.cleanup_session(node, false).await;
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
