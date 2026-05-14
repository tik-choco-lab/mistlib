use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::stats::STATS;
use mistlib_core::transport::NetworkEvent;
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;

use tokio_util::sync::CancellationToken;

pub struct Peer {
    pub pc: Arc<RTCPeerConnection>,
    pub channels: Arc<RwLock<HashMap<DeliveryMethod, Arc<RTCDataChannel>>>>,
    pub cancel_token: CancellationToken,
}

/// Shared transport-level state passed into peer handler setup functions.
#[derive(Clone)]
pub struct PeerSharedHandles {
    pub connection_states: Arc<StdRwLock<HashMap<NodeId, ConnectionState>>>,
    pub peers: Arc<RwLock<HashMap<NodeId, Arc<Peer>>>>,
    pub pending_candidates: Arc<RwLock<HashMap<NodeId, Vec<String>>>>,
    pub connection_attempt_ids: Arc<StdRwLock<HashMap<NodeId, u32>>>,
    pub last_disconnect_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
}

impl PeerSharedHandles {
    pub async fn cleanup_session(&self, node: &NodeId, _force_failed: bool) {
        let had_attempt = {
            let mut attempts = self.connection_attempt_ids.write().unwrap();
            attempts.remove(node).is_some()
        };
        let had_state = {
            let mut states = self.connection_states.write().unwrap();
            states.remove(node).is_some()
        };
        let peer = {
            let mut peers = self.peers.write().await;
            peers.remove(node)
        };
        let had_pending_candidates = {
            let mut pc_lock = self.pending_candidates.write().await;
            pc_lock.remove(node).is_some()
        };

        let had_peer = peer.is_some();
        let had_session_state = had_attempt || had_state || had_peer || had_pending_candidates;
        if had_session_state {
            let mut last_disconnect = self.last_disconnect_at.write().unwrap();
            last_disconnect.insert(node.clone(), std::time::Instant::now());
        }

        if let Some(peer) = peer {
            peer.close_all().await;
            crate::mem::record_peer_cleaned();
        }

        if had_session_state {
            crate::events::on_disconnected_internal(node.clone());
        }
    }
}

impl Peer {
    pub async fn close_all(&self) {
        self.cancel_token.cancel();
        self.detach_peer_handlers();
        let channels = {
            let mut dc_lock = self.channels.write().await;
            std::mem::take(&mut *dc_lock)
        };

        for (_, dc) in channels {
            Self::detach_data_channel_handlers(&dc);
            let _ = dc.close().await;
        }
        let _ = self.pc.close().await;
        tracing::info!(
            "[MEM] close_all pc strong_count={}",
            Arc::strong_count(&self.pc)
        );
    }

    fn detach_peer_handlers(&self) {
        self.pc.on_ice_candidate(Box::new(|_| Box::pin(async {})));
        self.pc.on_data_channel(Box::new(|_| Box::pin(async {})));
        self.pc
            .on_peer_connection_state_change(Box::new(|_| Box::pin(async {})));
    }

    fn detach_data_channel_handlers(dc: &RTCDataChannel) {
        dc.on_open(Box::new(|| Box::pin(async {})));
        dc.on_close(Box::new(|| Box::pin(async {})));
        dc.on_message(Box::new(|_| Box::pin(async {})));
    }

    pub async fn setup_handlers(
        self: &Arc<Self>,
        remote_id: NodeId,
        signaler: Arc<dyn Signaler>,
        local_id: NodeId,
        room_id: String,
        event_tx: Option<mpsc::Sender<NetworkEvent>>,
        handles: PeerSharedHandles,
    ) -> crate::error::Result<()> {
        self.setup_ice_candidate_handler(remote_id.clone(), signaler, local_id, room_id);
        self.setup_data_channel_handler(remote_id.clone(), event_tx, handles.clone());
        self.setup_connection_state_handler(remote_id, handles);
        Ok(())
    }

    fn setup_ice_candidate_handler(
        self: &Arc<Self>,
        remote_id: NodeId,
        signaler: Arc<dyn Signaler>,
        local_id: NodeId,
        room_id: String,
    ) {
        let pc_weak = Arc::downgrade(&self.pc);
        self.pc.on_ice_candidate(Box::new(
            move |candidate: Option<webrtc::ice_transport::ice_candidate::RTCIceCandidate>| {
                let signaler = signaler.clone();
                let local_id = local_id.clone();
                let remote_id = remote_id.clone();
                let room_id = room_id.clone();
                let pc_weak = pc_weak.clone();

                Box::pin(async move {
                    if pc_weak.strong_count() == 0 {
                        return;
                    }

                    let Some(cand) = candidate else { return };
                    let Ok(json) = cand.to_json() else { return };

                    let data = serde_json::to_string(&json).unwrap_or_default();
                    let _ = signaler
                        .send_signaling(
                            &remote_id,
                            MessageContent::Data(SignalingData {
                                sender_id: local_id,
                                receiver_id: remote_id.clone(),
                                room_id,
                                data,
                                signaling_type: SignalingType::Candidate,
                            }),
                        )
                        .await;
                })
            },
        ));
    }

    fn setup_data_channel_handler(
        self: &Arc<Self>,
        remote_id: NodeId,
        event_tx: Option<mpsc::Sender<NetworkEvent>>,
        handles: PeerSharedHandles,
    ) {
        let peer_weak = Arc::downgrade(self);
        self.pc
            .on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
                let peer_weak = peer_weak.clone();
                let tx_opt = event_tx.clone();
                let remote_id = remote_id.clone();
                let handles = handles.clone();
                let label = dc.label().to_string();
                Box::pin(async move {
                    let Some(peer) = peer_weak.upgrade() else {
                        return;
                    };
                    if peer.cancel_token.is_cancelled() {
                        return;
                    }
                    let method = match label.as_str() {
                        "reliable" => DeliveryMethod::ReliableOrdered,
                        "unreliable-ordered" => DeliveryMethod::UnreliableOrdered,
                        "unreliable" => DeliveryMethod::Unreliable,
                        _ => DeliveryMethod::ReliableOrdered,
                    };
                    {
                        let mut dc_lock = peer.channels.write().await;
                        dc_lock.insert(method, dc.clone());
                    }
                    Self::setup_dc_handlers(
                        dc,
                        tx_opt,
                        remote_id,
                        peer.cancel_token.clone(),
                        handles,
                    )
                    .await;
                })
            }));
    }

    fn setup_connection_state_handler(
        self: &Arc<Self>,
        remote_id: NodeId,
        handles: PeerSharedHandles,
    ) {
        let cancel_token = self.cancel_token.clone();
        let connection_states_cb = handles.connection_states.clone();
        let remote_id_cb = remote_id.clone();
        let peers_cb_state_change = Arc::downgrade(&handles.peers);
        let pending_candidates_cb = handles.pending_candidates.clone();
        let attempts_for_state_change = handles.connection_attempt_ids.clone();
        let last_disconnect_at_cb = handles.last_disconnect_at.clone();
        self.pc
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                if cancel_token.is_cancelled() {
                    return Box::pin(async {});
                }

                let state = match s {
                    RTCPeerConnectionState::Connected => ConnectionState::Connected,
                    RTCPeerConnectionState::Connecting | RTCPeerConnectionState::New => {
                        ConnectionState::Connecting
                    }
                    RTCPeerConnectionState::Disconnected => ConnectionState::Disconnected,
                    RTCPeerConnectionState::Failed => ConnectionState::Failed,
                    _ => ConnectionState::Disconnected,
                };

                tracing::info!(
                    "[RUST] [{}] peer connection state changed: {:?}",
                    remote_id_cb,
                    s
                );

                {
                    let mut states = connection_states_cb.write().unwrap();
                    if state == ConnectionState::Disconnected || state == ConnectionState::Failed {
                        let had_state = states.contains_key(&remote_id_cb);
                        states.remove(&remote_id_cb);
                        tracing::debug!(
                            "[CS] REMOVE state_change({:?}): {} total={}",
                            s,
                            remote_id_cb,
                            states.len()
                        );

                        {
                            let mut attempts = attempts_for_state_change.write().unwrap();
                            attempts.remove(&remote_id_cb);
                        }
                        {
                            let mut last_disconnect = last_disconnect_at_cb.write().unwrap();
                            last_disconnect.insert(remote_id_cb.clone(), Instant::now());
                        }

                        let peers_weak = peers_cb_state_change.clone();
                        let pc_cb = pending_candidates_cb.clone();
                        let remote_id_cb_2 = remote_id_cb.clone();
                        tokio::spawn(async move {
                            if let Some(peers_cb) = peers_weak.upgrade() {
                                let peer_opt = {
                                    let mut peers_lock = peers_cb.write().await;
                                    peers_lock.remove(&remote_id_cb_2)
                                };
                                {
                                    let mut pc_lock = pc_cb.write().await;
                                    pc_lock.remove(&remote_id_cb_2);
                                }
                                if let Some(peer) = peer_opt {
                                    peer.close_all().await;
                                    crate::mem::record_peer_cleaned();
                                }
                            }
                        });

                        if had_state {
                            crate::events::on_disconnected_internal(remote_id_cb.clone());
                        }
                    } else if states.contains_key(&remote_id_cb) {
                        states.insert(remote_id_cb.clone(), state);
                        tracing::error!(
                            "[CS] INSERT state_change({:?}): {} total={}",
                            s,
                            remote_id_cb,
                            states.len()
                        );
                    } else {
                        tracing::warn!(
                            "[CS] IGNORE state_change({:?}) for unreserved peer {}",
                            s,
                            remote_id_cb
                        );
                    }
                }

                Box::pin(async move {})
            }));
    }

    pub async fn setup_dc_handlers(
        dc: Arc<RTCDataChannel>,
        event_tx: Option<mpsc::Sender<NetworkEvent>>,
        remote_id: NodeId,
        cancel_token: CancellationToken,
        handles: PeerSharedHandles,
    ) {
        Self::setup_dc_open_handler(
            &dc,
            remote_id.clone(),
            handles.connection_states.clone(),
            cancel_token.clone(),
        );
        Self::setup_dc_close_handler(&dc, remote_id.clone(), handles, cancel_token.clone());
        Self::setup_dc_message_handler(&dc, remote_id, event_tx, cancel_token);
    }

    fn setup_dc_open_handler(
        dc: &Arc<RTCDataChannel>,
        remote_id: NodeId,
        states: Arc<StdRwLock<HashMap<NodeId, ConnectionState>>>,
        cancel_token: CancellationToken,
    ) {
        dc.on_open(Box::new(move || {
            let remote_id = remote_id.clone();
            let states = states.clone();
            let cancel = cancel_token.clone();
            Box::pin(async move {
                if cancel.is_cancelled() {
                    return;
                }
                let mut lock = states.write().unwrap();
                if !lock.contains_key(&remote_id) {
                    tracing::warn!(
                        "[CS] IGNORE data_channel_open for unreserved peer {}",
                        remote_id
                    );
                    return;
                }
                lock.insert(remote_id.clone(), ConnectionState::Connected);
                let total = lock.len();
                drop(lock);
                tracing::info!(
                    "[Conn] DataChannel opened: {} -> Connected (total_connected={})",
                    remote_id.0,
                    total
                );
                crate::events::on_connected_internal(remote_id);
            })
        }));
    }

    fn setup_dc_close_handler(
        dc: &Arc<RTCDataChannel>,
        remote_id: NodeId,
        handles: PeerSharedHandles,
        cancel_token: CancellationToken,
    ) {
        let dc_for_close = dc.clone();
        dc.on_close(Box::new(move || {
            let remote_id = remote_id.clone();
            let states = handles.connection_states.clone();
            let peers = handles.peers.clone();
            let pending = handles.pending_candidates.clone();
            let attempts = handles.connection_attempt_ids.clone();
            let last_disconnect = handles.last_disconnect_at.clone();
            let dc = dc_for_close.clone();
            let cancel = cancel_token.clone();
            Box::pin(async move {
                if cancel.is_cancelled() {
                    return;
                }
                tracing::warn!("[RUST] [{}] data channel closed: {}", remote_id, dc.label());
                let peer = {
                    let mut lock = peers.write().await;
                    lock.remove(&remote_id)
                };
                let Some(peer) = peer else { return };
                {
                    let mut lock = attempts.write().unwrap();
                    lock.remove(&remote_id);
                }
                {
                    let mut lock = last_disconnect.write().unwrap();
                    lock.insert(remote_id.clone(), Instant::now());
                }
                {
                    let mut lock = states.write().unwrap();
                    lock.remove(&remote_id);
                }
                {
                    let mut lock = pending.write().await;
                    lock.remove(&remote_id);
                }
                peer.close_all().await;
                crate::mem::record_peer_cleaned();
                crate::events::on_disconnected_internal(remote_id);
            })
        }));
    }

    fn setup_dc_message_handler(
        dc: &Arc<RTCDataChannel>,
        remote_id: NodeId,
        event_tx: Option<mpsc::Sender<NetworkEvent>>,
        cancel_token: CancellationToken,
    ) {
        dc.on_message(Box::new(
            move |msg: webrtc::data_channel::data_channel_message::DataChannelMessage| {
                let tx = event_tx.clone();
                let remote_id = remote_id.clone();
                let cancel = cancel_token.clone();
                Box::pin(async move {
                    if cancel.is_cancelled() {
                        return;
                    }
                    STATS.add_receive_frame(&msg.data);
                    let Some(tx) = &tx else { return };
                    let _ = tx.try_send(NetworkEvent {
                        from: remote_id,
                        data: msg.data,
                    });
                })
            },
        ));
    }
}
