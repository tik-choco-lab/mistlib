use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use mistlib_core::signaling::{MessageContent, SignalingData, SignalingType};
use mistlib_core::transport::NetworkEvent;
use mistlib_core::types::{DeliveryMethod, NodeId};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::signaling_state::RTCSignalingState;

use super::{rollback_to_stable_on_failure, Peer, WebRtcTransport};

impl WebRtcTransport {
    pub(crate) async fn create_pc(&self, remote_id: NodeId) -> crate::error::Result<Arc<Peer>> {
        let ice_servers = self.ice_servers.read().unwrap().clone();
        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        let cancel_token = CancellationToken::new();
        let pc = Arc::new(self.api.new_peer_connection(config).await?);
        let channels: Arc<
            tokio::sync::RwLock<HashMap<DeliveryMethod, Arc<webrtc::data_channel::RTCDataChannel>>>,
        > = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let send_tx =
            Peer::spawn_send_queue(remote_id.clone(), channels.clone(), cancel_token.clone());
        let peer = Arc::new(Peer {
            pc: pc.clone(),
            channels,
            cancel_token: cancel_token.clone(),
            local_offer_unsent: std::sync::atomic::AtomicBool::new(false),
            negotiating: tokio::sync::Mutex::new(()),
            send_tx,
        });

        let event_tx = self.spawn_event_forwarder(cancel_token.clone());

        let media_tx = self.media_tx.lock().unwrap().clone();
        peer.setup_handlers(
            remote_id.clone(),
            self.signaler.clone(),
            self.local_node_id.clone(),
            self.get_room_id(),
            event_tx,
            self.peer_handles(),
            media_tx,
        )
        .await?;

        // New-peer hook (mirrors mistlib-wasm's `create_pc`): attach every
        // currently-published track to this brand-new `RTCPeerConnection`
        // before it produces its first offer/answer, so a late joiner needs
        // no extra app-level action to receive tracks already being
        // published into this room. `remote_id` is always brand new here (a
        // fresh `RTCPeerConnection` was just created above), so drop any
        // stale sender bookkeeping left over from a previous connection to
        // this same node -- otherwise `attach_published_tracks_to_peer`
        // would wrongly believe a track is already attached to this new `pc`
        // because it was attached to an earlier, now-replaced one.
        self.published_senders.write().await.remove(&remote_id);
        self.attach_published_tracks_to_peer(&remote_id, &peer)
            .await?;

        Ok(peer)
    }

    pub(crate) async fn acquire_handshake_permit(
        &self,
        node: &NodeId,
    ) -> mistlib_core::error::Result<()> {
        let permit = self
            .handshake_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;

        let mut permits = self.handshake_permits.write().unwrap();
        permits.insert(node.clone(), permit);
        Ok(())
    }

    pub(crate) fn has_active_session(&self, node: &NodeId) -> bool {
        let states = self.connection_states.read().unwrap();
        states.contains_key(node)
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
                    | Some(mistlib_core::types::ConnectionState::Reconnecting)
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
                Arc::downgrade(peer),
            )
            .await;

            let mut dc_lock = peer.channels.write().await;
            dc_lock.insert(method, dc);
        }

        Ok(())
    }

    async fn replace_peer_and_close_old(&self, node: &NodeId, peer: Arc<Peer>) {
        let send_tx = peer.send_tx.clone();
        let old_peer = {
            let mut peers = self.peers.write().await;
            peers.insert(node.clone(), peer)
        };
        // Mirror the swap into `send_queues` -- see
        // `WebRtcTransport::send_queues`'s doc comment. Inserted after the
        // `peers` insert above, per its ordering note.
        self.send_queues
            .write()
            .unwrap()
            .insert(node.clone(), send_tx);
        if let Some(old_peer) = old_peer {
            tracing::warn!("[WebRTC Close] reason=replace_peer node={}", node);
            old_peer.close_all().await;
            crate::mem::record_peer_cleaned();
        }
        crate::mem::record_peer_inserted();
    }

    /// Creates a fresh offer on `peer` and sends it over signaling. `pub(crate)`
    /// (rather than module-private) so both `signaling::handle_offer` (the
    /// follow-up renegotiation after answering a brand-new peer that just got
    /// published tracks attached -- see the comment there) and
    /// `publish::publish_local_track`/`unpublish_local_track` can drive
    /// renegotiation the same way `add_track_and_renegotiate` already does.
    pub(crate) async fn send_offer(
        &self,
        node: &NodeId,
        peer: &Arc<Peer>,
    ) -> mistlib_core::error::Result<()> {
        // Held for the whole create-offer -> apply -> send sequence below, not
        // just the precondition check -- see `Peer::negotiating`'s doc
        // comment for the race this closes (a concurrent `apply_offer` or
        // another `send_offer` on this same peer, e.g. via the overlay
        // signaling path's per-message `tokio::spawn` in
        // `MistEngine::handle_message_content`).
        let _negotiating = peer.negotiating.lock().await;

        let signaling_state = peer.pc.signaling_state();
        let pc_state = peer.pc.connection_state();
        // A previous offer that was applied locally but never delivered
        // (failed signaling send, see the `Err` branch below) is re-offerable
        // even though signaling is `HaveLocalOffer`: webrtc-rs has no rollback
        // to get back to `Stable`, but `HaveLocalOffer -> SetLocal(offer)` is
        // a valid transition, and no answer to the lost offer can ever arrive.
        // See `Peer::local_offer_unsent`'s doc for the full contract.
        let reoffer_after_lost_send = signaling_state == RTCSignalingState::HaveLocalOffer
            && peer
                .local_offer_unsent
                .load(std::sync::atomic::Ordering::SeqCst);
        if !reoffer_after_lost_send && !self.can_send_offer(node, pc_state, signaling_state) {
            return Err(mistlib_core::error::MistError::Internal(
                "Offer precondition failed".to_string(),
            ));
        }

        let offer = peer
            .pc
            .create_offer(None)
            .await
            .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;
        if let Err(e) = peer.pc.set_local_description(offer).await {
            rollback_to_stable_on_failure(&peer.pc, node).await;
            return Err(mistlib_core::error::MistError::Internal(e.to_string()));
        }

        if let Some(offer_desc) = peer.pc.local_description().await {
            let data = offer_desc.sdp;
            if let Err(e) = self
                .signaler
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
                .await
            {
                // The local offer is already applied (signaling_state is
                // HaveLocalOffer) even though it never reached the remote --
                // e.g. `RoutedSignaler` returning `RouteNotFound` because the
                // overlay route to this exact peer hasn't caught up with a
                // just-established connection yet (routing table sync runs
                // on a ~1s tick). The rollback below is best-effort only
                // (webrtc-rs 0.13 rejects every rollback transition -- see
                // `Peer::local_offer_unsent`'s doc); the flag is what actually
                // un-wedges the peer, by letting the next `send_offer` re-offer
                // from `HaveLocalOffer`.
                peer.local_offer_unsent
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                rollback_to_stable_on_failure(&peer.pc, node).await;
                return Err(e);
            }
        }
        peer.local_offer_unsent
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Adds a local media track to an already-connected peer and renegotiates
    /// (sends a fresh offer) so the remote side actually receives it.
    /// `Peer::add_local_track` alone only updates the local `PeerConnection`
    /// — WebRTC requires a new offer/answer exchange after a track is added
    /// post-connection for the remote to see it; nothing does that
    /// automatically (no `on_negotiation_needed` handler is registered).
    pub async fn add_track_and_renegotiate(
        &self,
        node: &NodeId,
        track: Arc<dyn webrtc::track::track_local::TrackLocal + Send + Sync>,
    ) -> crate::error::Result<Arc<webrtc::rtp_transceiver::rtp_sender::RTCRtpSender>> {
        let peer = {
            let peers = self.peers.read().await;
            peers.get(node).cloned()
        }
        .ok_or_else(|| crate::error::MistError::Internal(format!("Node not found: {:?}", node)))?;

        let sender = peer.add_local_track(track).await?;
        self.send_offer(node, &peer)
            .await
            .map_err(crate::error::MistError::Core)?;
        Ok(sender)
    }

    pub(super) async fn connect_inner(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        self.acquire_handshake_permit(node).await?;

        if !self.has_active_session(node) {
            self.handshake_permits.write().unwrap().remove(node);
            return Ok(());
        }

        let mut created_peer: Option<Arc<Peer>> = None;
        let result: mistlib_core::error::Result<()> = async {
            let peer = self
                .create_pc(node.clone())
                .await
                .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;
            created_peer = Some(peer.clone());

            let attempt_id = self.reserve_connection_attempt(node);
            // `[ConnTiming]` instrumentation: attempt-start timestamp,
            // overwritten on every fresh attempt for this node -- see
            // `WebRtcTransport::connect_started_at`'s doc comment.
            self.connect_started_at
                .write()
                .unwrap()
                .insert(node.clone(), Instant::now());
            // `[ConnTiming]` instrumentation: the offering side's connection
            // attempt is starting right now, at the same point
            // `connect_started_at` is stamped.
            super::conn_timing::log_attempt_start(node);
            self.spawn_connection_watchdog(node.clone(), attempt_id);

            self.setup_outgoing_data_channels(&peer, node).await?;
            self.replace_peer_and_close_old(node, peer.clone()).await;
            self.send_offer(node, &peer).await?;
            // Offer resend, initiator side: only the fresh connect_inner
            // attempt path opts into this -- `send_offer` is also called by
            // renegotiation/ICE-restart paths on an already-established
            // session, which must NOT get a resend (see
            // `sweeper::spawn_offer_resend`'s doc comment for the full
            // mechanism and the guards that make a stale resend a no-op).
            self.spawn_offer_resend(node.clone(), attempt_id, peer.clone());
            Ok(())
        }
        .await;

        if result.is_err() {
            // Guard on the peer this attempt actually created (if any): a
            // concurrent `handle_offer` for the same `NodeId` racing this
            // failed attempt may have already installed its own, healthy
            // peer by the time we get here. An unconditional-by-NodeId
            // cleanup would delete that live registration instead of this
            // attempt's own (failed) one -- see
            // `PeerSharedHandles::cleanup_session_if_current`'s doc comment.
            match &created_peer {
                Some(peer) => {
                    let expected = Arc::downgrade(peer);
                    self.cleanup_session_if_current(node, &expected, true, "connect_inner_error")
                        .await;
                }
                None => {
                    self.cleanup_session_with_reason(node, true, "connect_inner_error")
                        .await;
                }
            }
        }

        result
    }
}
