use async_trait::async_trait;
use bytes::Bytes;
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::stats::STATS;
use mistlib_core::transport::{NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::api::API;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

const SERVER_ID: &str = "server";
const CONNECTION_TIMEOUT_MS: u64 = 6000;
const RECONNECT_COOLDOWN_MS: u64 = 3000;
const LAST_DISCONNECT_TTL_MS: u64 = RECONNECT_COOLDOWN_MS;

pub mod connection;
pub mod peer;
pub mod signaling;
pub mod stats;
pub mod sweeper;

pub use peer::Peer;
pub use stats::SctpPeerStats;

use peer::PeerSharedHandles;

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
    pub(crate) sweeper_cancel: Mutex<Option<CancellationToken>>,
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
            sweeper_cancel: Mutex::new(None),
        }
    }

    pub fn set_room_id(&self, room_id: String) {
        let mut room = self.room_id.write().unwrap();
        *room = room_id;
    }

    pub fn set_max_connections(&self, max: u32) {
        self.max_connections.store(max, Ordering::Relaxed);
    }

    pub(crate) fn get_room_id(&self) -> String {
        self.room_id.read().unwrap().clone()
    }

    pub(crate) fn peer_handles(&self) -> PeerSharedHandles {
        PeerSharedHandles {
            connection_states: self.connection_states.clone(),
            peers: self.peers.clone(),
            pending_candidates: self.pending_candidates.clone(),
            connection_attempt_ids: self.connection_attempt_ids.clone(),
            last_disconnect_at: self.last_disconnect_at.clone(),
        }
    }

    pub(crate) async fn cleanup_session(&self, node: &NodeId, force_failed: bool) {
        self.peer_handles()
            .cleanup_session(node, force_failed)
            .await;
    }

    pub async fn close_all_peer_connections(&self) {
        let peers = {
            let mut lock = self.peers.write().await;
            std::mem::take(&mut *lock)
        };

        for (_, peer) in peers {
            peer.close_all().await;
            crate::mem::record_peer_cleaned();
        }

        self.pending_candidates.write().await.clear();
        self.connection_attempt_ids.write().unwrap().clear();
        self.connection_states.write().unwrap().clear();
        self.last_disconnect_at.write().unwrap().clear();
    }
}

impl WebRtcTransport {
    /// シグナリングサーバーへ参加通知を送る。
    /// `start()` の後、ルームに参加する準備が整った時点で明示的に呼ぶこと。
    pub async fn announce_to_room(&self) -> mistlib_core::error::Result<()> {
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
            .await
    }
}

#[async_trait]
impl Transport for WebRtcTransport {
    async fn start(
        &self,
        handler: Arc<dyn NetworkEventHandler>,
    ) -> mistlib_core::error::Result<()> {
        self.ensure_session_sweeper();
        let mut h = self.event_handler.lock().unwrap();
        *h = Some(handler);
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
            STATS.add_send_frame(&data);
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
            tracing::debug!("[CS] INSERT connect: {} total={}", node, states.len());
        }

        let result = self.connect_inner(node).await;
        if result.is_err() {
            let states = self.connection_states.read().unwrap();
            tracing::debug!("[CS] FAILED connect_err: {} total={}", node, states.len());
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
