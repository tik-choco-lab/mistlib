use super::WebRtcTransport;
use async_trait::async_trait;
use mistlib_core::signaling::{MessageContent, SignalingData, SignalingHandler, SignalingType};
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, NodeId};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::signaling_state::RTCSignalingState;

impl WebRtcTransport {
    pub(crate) async fn handle_offer(
        &self,
        remote_id: NodeId,
        sdp: String,
    ) -> crate::error::Result<()> {
        let mut newly_reserved = false;

        {
            let mut states = self.connection_states.write().unwrap();
            if !states.contains_key(&remote_id) {
                let max = self
                    .max_connections
                    .load(std::sync::atomic::Ordering::Relaxed) as usize;
                let count = states
                    .values()
                    .filter(|s| {
                        **s == ConnectionState::Connected || **s == ConnectionState::Connecting
                    })
                    .count();
                if count >= max {
                    return Ok(());
                }
                states.insert(remote_id.clone(), ConnectionState::Connecting);
                tracing::warn!(
                    "[CS] INSERT handle_offer: {} total={}",
                    remote_id,
                    states.len()
                );
                newly_reserved = true;
            }
        }

        let peer = match self.create_pc(remote_id.clone()).await {
            Ok(p) => p,
            Err(e) => {
                if newly_reserved {
                    let mut states = self.connection_states.write().unwrap();
                    states.remove(&remote_id);
                    tracing::warn!(
                        "[CS] REMOVE handle_offer_create_err: {} total={}",
                        remote_id,
                        states.len()
                    );
                }
                return Err(e);
            }
        };

        {
            let mut peers = self.peers.write().await;
            peers.insert(remote_id.clone(), peer.clone());
        }

        let attempt_id = self.reserve_connection_attempt(&remote_id);
        self.spawn_connection_watchdog(remote_id.clone(), attempt_id);

        let result: crate::error::Result<()> = async {
            if peer.pc.signaling_state() != RTCSignalingState::Stable {
                return Err(crate::error::MistError::Internal(
                    "Offer precondition failed: signaling is not stable".to_string(),
                ));
            }

            let offer = serde_json::from_str::<RTCSessionDescription>(&sdp)?;
            peer.pc.set_remote_description(offer).await?;

            let answer = peer.pc.create_answer(None).await?;
            peer.pc.set_local_description(answer).await?;

            let cands = {
                let mut pc_lock = self.pending_candidates.write().await;
                pc_lock.remove(&remote_id)
            };

            if let Some(cands) = cands {
                for cand_json in cands {
                    if let Ok(candidate) = serde_json::from_str::<RTCIceCandidateInit>(&cand_json) {
                        let _ = peer.pc.add_ice_candidate(candidate).await;
                    }
                }
            }

            if let Some(answer_desc) = peer.pc.local_description().await {
                let data = serde_json::to_string(&answer_desc)?;
                self.signaler
                    .send_signaling(
                        &remote_id,
                        MessageContent::Data(SignalingData {
                            sender_id: self.local_node_id.clone(),
                            receiver_id: remote_id.clone(),
                            room_id: self.get_room_id(),
                            data,
                            signaling_type: SignalingType::Answer,
                        }),
                    )
                    .await
                    .map_err(|e| crate::error::MistError::Core(e))?;
            }
            Ok(())
        }
        .await;

        if result.is_err() {
            self.cleanup_session(&remote_id, true).await;
        }

        result
    }

    pub(crate) async fn handle_answer(
        &self,
        remote_id: NodeId,
        sdp: String,
    ) -> crate::error::Result<()> {
        let peer = {
            let peers = self.peers.read().await;
            peers.get(&remote_id).cloned()
        };

        if let Some(peer) = peer {
            let signaling_state = peer.pc.signaling_state();
            if signaling_state != RTCSignalingState::HaveLocalOffer {
                return Err(crate::error::MistError::Internal(format!(
                    "Answer precondition failed: signaling_state={:?}",
                    signaling_state
                )));
            }

            let answer = serde_json::from_str::<RTCSessionDescription>(&sdp)?;
            peer.pc.set_remote_description(answer).await?;

            let cands = {
                let mut pc_lock = self.pending_candidates.write().await;
                pc_lock.remove(&remote_id)
            };

            if let Some(cands) = cands {
                for cand_json in cands {
                    if let Ok(candidate) = serde_json::from_str::<RTCIceCandidateInit>(&cand_json) {
                        let _ = peer.pc.add_ice_candidate(candidate).await;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn handle_candidate(
        &self,
        remote_id: NodeId,
        cand_json: String,
    ) -> crate::error::Result<()> {
        let peer = {
            let peers = self.peers.read().await;
            peers.get(&remote_id).cloned()
        };

        if let Some(peer) = peer {
            if peer.pc.remote_description().await.is_some() {
                let candidate = serde_json::from_str::<RTCIceCandidateInit>(&cand_json)?;
                peer.pc.add_ice_candidate(candidate).await?;
                return Ok(());
            }
        }

        let mut pc_lock = self.pending_candidates.write().await;
        pc_lock.entry(remote_id).or_default().push(cand_json);
        Ok(())
    }
}

#[async_trait]
impl SignalingHandler for WebRtcTransport {
    async fn handle_message(&self, msg: MessageContent) -> mistlib_core::error::Result<()> {
        let data = match msg {
            MessageContent::Data(d) => d,
            _ => return Ok(()),
        };

        let current_room_id = self.get_room_id();
        if !data.room_id.is_empty() && data.room_id != current_room_id {
            tracing::warn!(
                "WebRtcTransport: ignore signaling from different room_id {} (current={})",
                data.room_id,
                current_room_id
            );
            return Ok(());
        }

        match data.signaling_type {
            SignalingType::Offer => self
                .handle_offer(data.sender_id.clone(), data.data)
                .await
                .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string())),
            SignalingType::Answer => self
                .handle_answer(data.sender_id.clone(), data.data)
                .await
                .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string())),
            SignalingType::Candidate => self
                .handle_candidate(data.sender_id.clone(), data.data)
                .await
                .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string())),
            SignalingType::Candidates => {
                let candidates: Vec<String> =
                    serde_json::from_str(&data.data).map_err(|e: serde_json::Error| {
                        mistlib_core::error::MistError::Internal(e.to_string())
                    })?;
                for cand in candidates {
                    let _ = self.handle_candidate(data.sender_id.clone(), cand).await;
                }
                Ok(())
            }
            SignalingType::Request => {
                if self.local_node_id != data.sender_id && self.local_node_id.0 < data.sender_id.0 {
                    let _ = self.connect(&data.sender_id).await;
                }
                Ok(())
            }
        }
    }
}
