use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use mistlib_core::signaling::{MessageContent, SignalingData, SignalingType};
use mistlib_core::transport::NetworkEvent;
use mistlib_core::types::{DeliveryMethod, NodeId};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::signaling_state::RTCSignalingState;

use super::peer::PeerSharedHandles;
use super::{Peer, WebRtcTransport};

impl WebRtcTransport {
    pub(crate) async fn create_pc(&self, remote_id: NodeId) -> crate::error::Result<Arc<Peer>> {
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_string()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let cancel_token = CancellationToken::new();
        let pc = Arc::new(self.api.new_peer_connection(config).await?);
        let peer = Arc::new(Peer {
            pc: pc.clone(),
            channels: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            cancel_token: cancel_token.clone(),
        });

        let event_tx = self.spawn_event_forwarder(cancel_token.clone());

        peer.setup_handlers(
            remote_id,
            self.signaler.clone(),
            self.local_node_id.clone(),
            self.get_room_id(),
            event_tx,
            PeerSharedHandles {
                connection_states: self.connection_states.clone(),
                peers: self.peers.clone(),
                pending_candidates: self.pending_candidates.clone(),
                connection_attempt_ids: self.connection_attempt_ids.clone(),
                last_disconnect_at: self.last_disconnect_at.clone(),
            },
        )
        .await?;

        Ok(peer)
    }

    pub(crate) fn reserve_connection_attempt(&self, node: &NodeId) -> u32 {
        let attempt_id = self
            .next_connection_attempt_id
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let mut attempts = self.connection_attempt_ids.write().unwrap();
        attempts.insert(node.clone(), attempt_id);
        attempt_id
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
                Some(mistlib_core::types::ConnectionState::Connecting)
                    | Some(mistlib_core::types::ConnectionState::Connected)
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

    fn spawn_event_forwarder(
        &self,
        cancel_token: CancellationToken,
    ) -> Option<tokio::sync::mpsc::Sender<NetworkEvent>> {
        let handler_opt = {
            let h = self.event_handler.lock().unwrap();
            h.clone()
        };

        handler_opt.map(|handler| {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<NetworkEvent>(2048);
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel_token.cancelled() => break,
                        Some(event) = rx.recv() => { handler.on_event(event); }
                        else => break,
                    }
                }
            });
            tx
        })
    }

    async fn setup_outgoing_data_channels(
        &self,
        peer: &Arc<Peer>,
        node: &NodeId,
    ) -> mistlib_core::error::Result<()> {
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

            let event_tx = self.spawn_event_forwarder(peer.cancel_token.clone());
            Peer::setup_dc_handlers(
                dc.clone(),
                event_tx,
                node.clone(),
                peer.cancel_token.clone(),
                self.peer_handles(),
            )
            .await;

            let mut dc_lock = peer.channels.write().await;
            dc_lock.insert(method, dc);
        }

        Ok(())
    }

    async fn replace_peer_and_close_old(&self, node: &NodeId, peer: Arc<Peer>) {
        let old_peer = {
            let mut peers = self.peers.write().await;
            peers.insert(node.clone(), peer)
        };
        if let Some(old_peer) = old_peer {
            old_peer.close_all().await;
            crate::mem::record_peer_cleaned();
        }
        crate::mem::record_peer_inserted();
    }

    async fn send_offer(&self, node: &NodeId, peer: &Arc<Peer>) -> mistlib_core::error::Result<()> {
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

    pub(super) async fn connect_inner(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        let peer = self
            .create_pc(node.clone())
            .await
            .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;

        let attempt_id = self.reserve_connection_attempt(node);
        self.spawn_connection_watchdog(node.clone(), attempt_id);

        self.setup_outgoing_data_channels(&peer, node).await?;
        self.replace_peer_and_close_old(node, peer.clone()).await;

        let result = self.send_offer(node, &peer).await;

        if result.is_err() {
            self.cleanup_session(node, true).await;
        }

        result
    }
}
