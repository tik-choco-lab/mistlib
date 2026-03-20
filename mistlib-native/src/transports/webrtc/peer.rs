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

impl Peer {
    pub async fn close_all(&self) {
        self.cancel_token.cancel();
        let channels = {
            let mut dc_lock = self.channels.write().await;
            std::mem::take(&mut *dc_lock)
        };

        for (_, dc) in channels {
            let _ = dc.close().await;
        }
        let _ = self.pc.close().await;
    }

    pub async fn setup_handlers(
        self: &Arc<Self>,
        remote_id: NodeId,
        signaler: Arc<dyn Signaler>,
        local_id: NodeId,
        room_id: String,
        event_tx: Option<mpsc::Sender<NetworkEvent>>,
        connection_states: Arc<StdRwLock<HashMap<NodeId, ConnectionState>>>,
        peers: Arc<RwLock<HashMap<NodeId, Arc<Peer>>>>,
        pending_candidates: Arc<RwLock<HashMap<NodeId, Vec<String>>>>,
        connection_attempt_ids: Arc<StdRwLock<HashMap<NodeId, u32>>>,
        last_disconnect_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    ) -> crate::error::Result<()> {
        let remote_id_cand = remote_id.clone();
        let signaler_cand = signaler.clone();
        let local_id_cand = local_id.clone();
        let room_id_cand = room_id.clone();

        let pc_weak = Arc::downgrade(&self.pc);
        self.pc.on_ice_candidate(Box::new(
            move |candidate: Option<webrtc::ice_transport::ice_candidate::RTCIceCandidate>| {
                let signaler = signaler_cand.clone();
                let local_id = local_id_cand.clone();
                let remote_id = remote_id_cand.clone();
                let room_id = room_id_cand.clone();
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

        let peer_weak = Arc::downgrade(self);
        let event_tx_dc = event_tx.clone();
        let remote_id_dc = remote_id.clone();
        let connection_states_dc = connection_states.clone();
        let peers_dc = peers.clone();
        let pending_candidates_dc = pending_candidates.clone();
        let attempts_dc = connection_attempt_ids.clone();
        let last_disconnect_at_dc = last_disconnect_at.clone();

        self.pc
            .on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
                let peer_weak = peer_weak.clone();
                let tx_opt = event_tx_dc.clone();
                let remote_id = remote_id_dc.clone();
                let connection_states = connection_states_dc.clone();
                let peers = peers_dc.clone();
                let pending_candidates = pending_candidates_dc.clone();
                let attempts = attempts_dc.clone();
                let last_disconnect_at = last_disconnect_at_dc.clone();
                let label = dc.label().to_string();
                Box::pin(async move {
                    let Some(peer) = peer_weak.upgrade() else {
                        return;
                    };
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
                        connection_states,
                        peers,
                        pending_candidates,
                        attempts,
                        last_disconnect_at,
                    )
                    .await;
                })
            }));

        let connection_states_cb = connection_states.clone();
        let remote_id_cb = remote_id.clone();
        let peers_cb_state_change = Arc::downgrade(&peers);
        let pending_candidates_cb = pending_candidates.clone();
        let attempts_for_state_change = connection_attempt_ids.clone();
        let last_disconnect_at_cb = last_disconnect_at.clone();
        self.pc
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                let state = match s {
                    RTCPeerConnectionState::Connected => ConnectionState::Connecting,
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
                        tracing::error!(
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
                                    let mut pc_lock = pc_cb.write().await;
                                    pc_lock.remove(&remote_id_cb_2);
                                    let mut peers_lock = peers_cb.write().await;
                                    peers_lock.remove(&remote_id_cb_2)
                                };
                                if let Some(peer) = peer_opt {
                                    peer.close_all().await;
                                }
                            }
                        });

                        if had_state {
                            crate::events::on_disconnected_internal(remote_id_cb.clone());
                        }
                    } else {
                        states.insert(remote_id_cb.clone(), state);
                        tracing::error!(
                            "[CS] INSERT state_change({:?}): {} total={}",
                            s,
                            remote_id_cb,
                            states.len()
                        );
                    }
                }

                Box::pin(async move {})
            }));

        Ok(())
    }

    pub async fn setup_dc_handlers(
        dc: Arc<RTCDataChannel>,
        event_tx: Option<mpsc::Sender<NetworkEvent>>,
        remote_id: NodeId,
        cancel_token: CancellationToken,
        connection_states: Arc<StdRwLock<HashMap<NodeId, ConnectionState>>>,
        peers: Arc<RwLock<HashMap<NodeId, Arc<Peer>>>>,
        pending_candidates: Arc<RwLock<HashMap<NodeId, Vec<String>>>>,
        connection_attempt_ids: Arc<StdRwLock<HashMap<NodeId, u32>>>,
        last_disconnect_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    ) {
        let remote_id_open = remote_id.clone();
        let states_open = connection_states.clone();
        dc.on_open(Box::new(move || {
            let remote_id = remote_id_open.clone();
            let states = states_open.clone();
            Box::pin(async move {
                let mut lock = states.write().unwrap();
                lock.insert(remote_id.clone(), ConnectionState::Connected);
                drop(lock);
                crate::events::on_connected_internal(remote_id);
            })
        }));

        let remote_id_close = remote_id.clone();
        let states_close = connection_states.clone();
        let peers_close = peers.clone();
        let pending_close = pending_candidates.clone();
        let attempts_close = connection_attempt_ids.clone();
        let last_disconnect_close = last_disconnect_at.clone();
        let dc_for_close = dc.clone();
        dc.on_close(Box::new(move || {
            let remote_id = remote_id_close.clone();
            let states = states_close.clone();
            let peers = peers_close.clone();
            let pending = pending_close.clone();
            let attempts = attempts_close.clone();
            let last_disconnect = last_disconnect_close.clone();
            let dc = dc_for_close.clone();
            Box::pin(async move {
                tracing::warn!("[RUST] [{}] data channel closed: {}", remote_id, dc.label());
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
                    lock.insert(remote_id.clone(), ConnectionState::Failed);
                }
                let peer = {
                    let mut lock = peers.write().await;
                    lock.remove(&remote_id)
                };
                {
                    let mut lock = pending.write().await;
                    lock.remove(&remote_id);
                }
                if let Some(peer) = peer {
                    peer.close_all().await;
                }
                crate::events::on_disconnected_internal(remote_id);
            })
        }));

        let remote_id_msg = remote_id.clone();
        let tx_msg = event_tx;
        dc.on_message(Box::new(
            move |msg: webrtc::data_channel::data_channel_message::DataChannelMessage| {
                let tx = tx_msg.clone();
                let remote_id = remote_id_msg.clone();
                let cancel = cancel_token.clone();
                Box::pin(async move {
                    if cancel.is_cancelled() {
                        return;
                    }
                    STATS.add_receive(msg.data.len() as u64);
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
