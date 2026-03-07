use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::stats::STATS;
use mistlib_core::transport::NetworkEvent;
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};
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

        let pc = self.pc.clone();
        tokio::spawn(async move {
            for (_, dc) in channels {
                let _ = dc.close().await;
            }
            let _ = pc.close().await;
        });
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

        self.pc
            .on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
                let peer_weak = peer_weak.clone();
                let tx_opt = event_tx_dc.clone();
                let remote_id = remote_id_dc.clone();
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
                    Self::setup_dc_handlers(dc, tx_opt, remote_id, peer.cancel_token.clone()).await;
                })
            }));

        let connection_states_cb = connection_states.clone();
        let remote_id_cb = remote_id.clone();
        let peers_cb_state_change = Arc::downgrade(&peers);
        let pending_candidates_cb = pending_candidates.clone();
        self.pc
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
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
                        states.remove(&remote_id_cb);
                        tracing::error!(
                            "[CS] REMOVE state_change({:?}): {} total={}",
                            s,
                            remote_id_cb,
                            states.len()
                        );

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

                if state == ConnectionState::Connected {
                    crate::events::on_connected_internal(remote_id_cb.clone());
                } else if state == ConnectionState::Disconnected || state == ConnectionState::Failed
                {
                    crate::events::on_disconnected_internal(remote_id_cb.clone());
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
    ) {
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
