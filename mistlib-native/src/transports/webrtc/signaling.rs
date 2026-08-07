use super::{
    backoff::exponential_backoff_ms, rollback_to_stable_on_failure, Peer, WebRtcTransport,
    CONNECT_REQUEST_RETRY_INITIAL_MS, CONNECT_REQUEST_RETRY_MAX_INTERVAL_MS,
    CONNECT_REQUEST_RETRY_MULTIPLIER, DEFAULT_CONNECT_REQUEST_RETRIES,
};
use async_trait::async_trait;
use mistlib_core::signaling::{
    CandidateAck, CandidateEnvelope, MessageContent, NegotiationAck, NegotiationEnvelope,
    SignalingData, SignalingHandler, SignalingType,
};
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, NodeId};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::signaling_state::RTCSignalingState;

impl WebRtcTransport {
    async fn send_negotiation_ack(&self, remote_id: &NodeId, room_id: &str, id: u64) {
        let Ok(payload) = serde_json::to_string(&NegotiationAck { id }) else {
            return;
        };
        if let Err(error) = self
            .signaler
            .send_signaling(
                remote_id,
                MessageContent::Data(SignalingData {
                    sender_id: self.local_node_id.clone(),
                    receiver_id: remote_id.clone(),
                    room_id: room_id.to_string(),
                    data: payload,
                    signaling_type: SignalingType::NegotiationAck,
                }),
            )
            .await
        {
            tracing::warn!(
                "Failed to send negotiation ACK to {} id={:016x}: {}",
                remote_id,
                id,
                error
            );
        }
    }

    fn connect_request_retry_limit() -> u32 {
        std::env::var("MIST_WEBRTC_CONNECT_REQUEST_RETRIES")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_CONNECT_REQUEST_RETRIES)
    }

    fn reserve_connect_request_attempt(&self, node: &NodeId) -> Option<u32> {
        let mut attempts = self.connect_request_attempt_ids.write().unwrap();
        if attempts.contains_key(node) {
            return None;
        }
        let attempt_id = self
            .next_connection_attempt_id
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        attempts.insert(node.clone(), attempt_id);
        Some(attempt_id)
    }

    pub(crate) fn clear_connect_request_attempt(&self, node: &NodeId) {
        self.connect_request_attempt_ids
            .write()
            .unwrap()
            .remove(node);
    }

    pub(crate) async fn request_lower_id_offer(
        &self,
        node: &NodeId,
    ) -> mistlib_core::error::Result<()> {
        {
            let peers = self.peers.read().await;
            if peers.contains_key(node) {
                return Ok(());
            }
        }
        if self.has_active_session(node) {
            return Ok(());
        }

        let Some(attempt_id) = self.reserve_connect_request_attempt(node) else {
            return Ok(());
        };

        if let Err(err) = self.send_connect_request(node).await {
            tracing::warn!(
                "[WebRTC Request] send_failed node={} attempt={} retry=1 error={:?}",
                node,
                attempt_id,
                err
            );
        }
        self.spawn_connect_request_retry(node.clone(), attempt_id);
        Ok(())
    }

    pub(crate) async fn send_connect_request(
        &self,
        node: &NodeId,
    ) -> mistlib_core::error::Result<()> {
        self.signaler
            .send_signaling(
                node,
                MessageContent::Data(SignalingData {
                    sender_id: self.local_node_id.clone(),
                    receiver_id: node.clone(),
                    room_id: self.get_room_id(),
                    data: String::new(),
                    signaling_type: SignalingType::Request,
                }),
            )
            .await
    }

    /// Spawns the background retry loop for a `CONNECT_REQUEST` nudge
    /// previously reserved (and already sent once) by
    /// `request_lower_id_offer`. Retries follow an exponential backoff
    /// (`CONNECT_REQUEST_RETRY_INITIAL_MS`, growing by
    /// `CONNECT_REQUEST_RETRY_MULTIPLIER` each time, capped at
    /// `CONNECT_REQUEST_RETRY_MAX_INTERVAL_MS`) instead of a fixed interval,
    /// so a burst of simultaneously-disconnected peers doesn't resynchronize
    /// into a retry storm hitting the signaling layer and
    /// `handshake_semaphore` at the same cadence.
    ///
    /// Once every retry is exhausted (or immediately, if `retry_limit <= 1`
    /// leaves nothing to retry) this clears `node`'s entry from
    /// `connect_request_attempt_ids` -- the *only* thing standing between a
    /// future `connect()`/`request_lower_id_offer` call for the same `node`
    /// and a fresh retry cycle is `reserve_connect_request_attempt`'s
    /// "already pending" check against that same map. Leaving a stale entry
    /// behind here (the `retry_limit <= 1` path used to do exactly that --
    /// returning before ever spawning the task that would otherwise clear
    /// it) would silently and permanently swallow every later
    /// `request_lower_id_offer` call for `node` as a no-op, with no log line
    /// to explain why: `reserve_connect_request_attempt` returns `None` the
    /// moment the map already contains the key.
    fn spawn_connect_request_retry(&self, node: NodeId, attempt_id: u32) {
        let retry_limit = Self::connect_request_retry_limit();
        if retry_limit <= 1 {
            let mut attempts = self.connect_request_attempt_ids.write().unwrap();
            if matches!(attempts.get(&node), Some(id) if *id == attempt_id) {
                attempts.remove(&node);
            }
            return;
        }

        let signaler = self.signaler.clone();
        let local_node_id = self.local_node_id.clone();
        let room_id = self.get_room_id();
        let pending_attempts = self.connect_request_attempt_ids.clone();
        let peers = self.peers.clone();
        let states = self.connection_states.clone();

        tokio::spawn(async move {
            for retry in 2..=retry_limit {
                // `retry` is the send this delay precedes (2 == the 2nd
                // send overall), so it maps to backoff attempt number
                // `retry - 1` (the 1st waited interval, `attempt_number ==
                // 1`, is `CONNECT_REQUEST_RETRY_INITIAL_MS` unchanged).
                let delay_ms = exponential_backoff_ms(
                    retry - 1,
                    CONNECT_REQUEST_RETRY_INITIAL_MS,
                    CONNECT_REQUEST_RETRY_MULTIPLIER,
                    CONNECT_REQUEST_RETRY_MAX_INTERVAL_MS,
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;

                let is_current_attempt = {
                    let attempts = pending_attempts.read().unwrap();
                    matches!(attempts.get(&node), Some(id) if *id == attempt_id)
                };
                if !is_current_attempt {
                    return;
                }

                let has_peer = {
                    let peers = peers.read().await;
                    peers.contains_key(&node)
                };
                let has_state = {
                    let states = states.read().unwrap();
                    states.contains_key(&node)
                };
                if has_peer || has_state {
                    pending_attempts.write().unwrap().remove(&node);
                    return;
                }

                let result = signaler
                    .send_signaling(
                        &node,
                        MessageContent::Data(SignalingData {
                            sender_id: local_node_id.clone(),
                            receiver_id: node.clone(),
                            room_id: room_id.clone(),
                            data: String::new(),
                            signaling_type: SignalingType::Request,
                        }),
                    )
                    .await;
                if let Err(err) = result {
                    tracing::warn!(
                        "[WebRTC Request] send_failed node={} attempt={} retry={} error={:?}",
                        node,
                        attempt_id,
                        retry,
                        err
                    );
                }
            }

            let still_pending = {
                let mut attempts = pending_attempts.write().unwrap();
                matches!(attempts.get(&node), Some(id) if *id == attempt_id)
                    .then(|| attempts.remove(&node))
                    .flatten()
                    .is_some()
            };
            if still_pending {
                tracing::warn!(
                    "[WebRTC Request] retry_exhausted node={} attempt={} retries={}",
                    node,
                    attempt_id,
                    retry_limit
                );
            }
        });
    }
}

/// Remote-takeover fix (measured on a 50-node fleet run: node A detects a
/// dead connection to B via its 6s watchdog and tries to re-establish, but B
/// often still believes the old connection is alive for a median of 75.7s,
/// sometimes never within a run's lifetime). Two independent pieces of
/// inbound signaling are each, on their own, evidence that the remote side
/// no longer has a working session with us -- rather than ignoring that
/// evidence and waiting for local detection (the connect-timeout watchdog,
/// the periodic sweeper), both of the impls below tear the stale session
/// down and cooperate with the remote's fresh attempt, subject to the guards
/// in `takeover_allowed` (`webrtc.rs`):
///
/// 1. **Change 1**: a `CONNECT_REQUEST` from a peer we, as the deterministic
///    offerer, already hold a `self.peers` entry for -- see
///    `SignalingHandler::handle_message`'s `SignalingType::Request` arm and
///    `maybe_takeover_for_connect_request` below. Left unhandled, this is
///    exactly the "mirror silent" failure mode (35.4% of a ~1700-timeout
///    sample): `Transport::connect`'s `if peers.contains_key(node) { return
///    Ok(()); }` fast path silently swallows every nudge.
/// 2. **Change 2**: an inbound offer for an already-known peer whose DTLS
///    fingerprint (`sdp_fingerprint`) differs from the existing session's --
///    see `handle_offer` and `should_takeover_on_fresh_offer` below. Left
///    unhandled, this is the "mirror still connected" failure mode (38.2% of
///    the same sample): the remote built a brand-new `RTCPeerConnection`
///    (fresh DTLS cert, fresh ICE ufrag) because it thinks the old one is
///    dead, but `apply_offer` would otherwise reuse our still-registered,
///    actually-dead `RTCPeerConnection` -- an implicit ICE restart on a
///    corpse whose success is entirely up to webrtc-rs internals and races
///    against that dead PC's own Failed/Closed cleanup.
impl WebRtcTransport {
    /// Shared guard inputs for both takeover call sites: whether `peer`
    /// currently looks healthy (the same check the sweeper uses,
    /// `has_required_data_channel`, combined with the live
    /// `pc.connection_state()`), how long ago (if ever) it was last
    /// confirmed established (`established_at`), and how long ago (if ever)
    /// an attempt to connect to this exact node started and hasn't resolved
    /// yet (`connect_started_at` -- see `WebRtcTransport::connect_started_at`'s
    /// doc comment: cleared once the ReliableOrdered DC opens or the connect
    /// watchdog fires, so a live entry here means an attempt is genuinely
    /// still in flight). See `takeover_allowed`'s doc comment (`webrtc.rs`)
    /// for how these feed its three guards.
    async fn takeover_guard_inputs(
        &self,
        node: &NodeId,
        peer: &Peer,
    ) -> (bool, Option<u64>, Option<u128>) {
        let healthy = peer.pc.connection_state() == RTCPeerConnectionState::Connected
            && Self::has_required_data_channel(peer).await;
        let ms_since_connected = self
            .established_at
            .read()
            .unwrap()
            .get(node)
            .map(|at| at.elapsed().as_millis() as u64);
        let ms_since_connect_started = self
            .connect_started_at
            .read()
            .unwrap()
            .get(node)
            .map(|at| at.elapsed().as_millis());
        (healthy, ms_since_connected, ms_since_connect_started)
    }

    /// The other guard input: how long ago (if ever) `node` was last forced
    /// through a takeover -- see `takeover_allowed`'s per-peer rate-limit
    /// rationale (`webrtc.rs`) and `WebRtcTransport::last_takeover_at`'s doc
    /// comment.
    fn ms_since_last_takeover(&self, node: &NodeId) -> Option<u64> {
        self.last_takeover_at
            .read()
            .unwrap()
            .get(node)
            .map(|at| at.elapsed().as_millis() as u64)
    }

    /// Tears down `node`'s existing session via the same forced-cleanup path
    /// already used for dead sessions (the connect-timeout watchdog, the
    /// periodic sweeper), tagging the `[ConnTiming]`/`[WebRTC Close]`
    /// disconnect event with `reason` so eval metrics can count takeovers
    /// separately from every other teardown cause (see
    /// `conn_timing::log_disconnect`'s contract -- `reason` must stay
    /// snake_case).
    ///
    /// Unlike every other caller of `cleanup_session_with_reason`, this path
    /// must NOT leave the ordinary reconnect cooldown (`last_disconnect_at`,
    /// ~3s) armed afterwards: the whole point of a takeover is to cooperate
    /// with the fresh attempt immediately rather than make it wait out a
    /// cooldown meant for unrelated reconnects. The per-peer
    /// `REMOTE_TAKEOVER_MIN_INTERVAL_MS` rate limit (`last_takeover_at`,
    /// stamped here) is what actually protects this path against a takeover
    /// storm.
    async fn takeover_stale_session(&self, node: &NodeId, reason: &'static str) {
        self.cleanup_session_with_reason(node, true, reason).await;
        self.last_disconnect_at.write().unwrap().remove(node);
        self.last_takeover_at
            .write()
            .unwrap()
            .insert(node.clone(), Instant::now());
    }

    /// Change 1: called from the `SignalingType::Request` handler whenever
    /// we (the deterministic offerer) still hold a `self.peers` entry for
    /// the requesting node. No-ops (falls through to today's behavior --
    /// `connect()`'s existing `if peers.contains_key(node) { return Ok(());
    /// }` no-op) when there is no existing peer to take over, or when
    /// `takeover_allowed`'s guards reject it.
    async fn maybe_takeover_for_connect_request(&self, node: &NodeId) {
        let peer = {
            let peers = self.peers.read().await;
            peers.get(node).cloned()
        };
        let Some(peer) = peer else {
            return;
        };

        let (healthy, ms_since_connected, ms_since_connect_started) =
            self.takeover_guard_inputs(node, &peer).await;
        let ms_since_last_takeover = self.ms_since_last_takeover(node);
        if !super::takeover_allowed(
            healthy,
            ms_since_connected,
            ms_since_last_takeover,
            ms_since_connect_started,
        ) {
            return;
        }

        tracing::warn!(
            "[RemoteTakeover] CONNECT_REQUEST from {} against a stale peer entry -- forcing \
             cleanup and cooperating with the fresh attempt",
            node
        );
        self.takeover_stale_session(node, "remote_connect_request_takeover")
            .await;
    }

    /// Change 2: called from `handle_offer` whenever an existing peer is
    /// found for the offer's sender, to decide whether `sdp` was produced by
    /// a fresh `RTCPeerConnection` (different DTLS fingerprint) that should
    /// take over from the one we still have registered, rather than being
    /// applied to it as an in-place renegotiation/ICE restart. See
    /// `offer_takeover_decision`'s doc comment for the full fingerprint
    /// decision table.
    async fn should_takeover_on_fresh_offer(&self, node: &NodeId, peer: &Peer, sdp: &str) -> bool {
        let existing_fingerprint = match peer.pc.remote_description().await {
            Some(desc) => sdp_fingerprint(&desc.sdp),
            None => None,
        };
        let incoming_fingerprint = sdp_fingerprint(sdp);

        let (healthy, ms_since_connected, ms_since_connect_started) =
            self.takeover_guard_inputs(node, peer).await;
        let ms_since_last_takeover = self.ms_since_last_takeover(node);

        let takeover = offer_takeover_decision(
            existing_fingerprint.as_deref(),
            incoming_fingerprint.as_deref(),
            healthy,
            ms_since_connected,
            ms_since_last_takeover,
            ms_since_connect_started,
        );
        if takeover {
            tracing::warn!(
                "[RemoteTakeover] offer from {} carries a different DTLS fingerprint than our \
                 existing session -- forcing cleanup and treating it as a fresh connection",
                node
            );
        }
        takeover
    }
}

/// Repair-first ICE restart, Change 3: the non-initiator side of a peer pair
/// has no PC-level ICE-restart trigger of its own (only the initiator ever
/// calls `create_offer(ice_restart: true)`, see `is_ice_restart_initiator`),
/// so instead of doing nothing while a disconnect grace runs, it sends a
/// `RestartRequest` nudge (`PeerSharedHandles::send_restart_request`) asking
/// the initiator to try. This is that request's receive-side handler.
impl WebRtcTransport {
    /// Handles an incoming `RestartRequest` (a `SignalingType::Request`
    /// tagged with `RESTART_REQUEST_MARKER`). Deliberately narrow: if `self`
    /// has an existing session for `sender_id`, attempt a rate-limited ICE
    /// restart on it (`PeerSharedHandles::maybe_try_ice_restart`); if not,
    /// ignore the request entirely. Unlike `SignalingType::Request`'s
    /// ordinary CONNECT_REQUEST handling (which can call `self.connect(..)`
    /// to initiate a brand-new session), this never creates a connection --
    /// reconnection initiation stays with the overlay balancer/CONNECT_REQUEST
    /// flow, exactly as the spec for this change requires. A `RestartRequest`
    /// for a peer we have no session for is just a stale/racing nudge (e.g.
    /// arriving after our own side already tore the session down for an
    /// unrelated reason) with nothing useful to repair.
    ///
    /// ICE restart as rescue, not reflex: this now debounces before acting,
    /// mirroring `PeerSharedHandles::spawn_repair_trigger`'s own
    /// debounce+jitter delay (`super::REPAIR_TRIGGER_DEBOUNCE_MS`/
    /// `super::repair_trigger_jitter_ms`) rather than honoring the request
    /// the instant it arrives. Honoring a `RestartRequest` instantly means
    /// executing a restart while the shared path fault that prompted the
    /// request may still be present; the delay lets short blips end (and the
    /// requester recover naturally) before we touch a working session that
    /// might not have needed touching at all.
    ///
    /// Deliberately does NOT gate on our own `disconnected_since`: the whole
    /// point of this nudge is the asymmetric case where the requester's side
    /// sees a broken path while our own side still looks perfectly healthy --
    /// gating on our own grace would make the nudge a no-op exactly when it's
    /// needed. The delay itself, plus the existing per-peer rate limit
    /// (`maybe_try_ice_restart`/`super::ice_restart_allowed`) and the grace
    /// re-arm it performs when a grace happens to be running on our side too
    /// (`PeerSharedHandles::rearm_disconnect_grace`), are the storm
    /// protection here instead.
    ///
    /// Spawned (like `spawn_repair_trigger`) rather than run inline: this
    /// runs on the signaling dispatch path
    /// (`SignalingHandler::handle_message`, itself invoked from a per-message
    /// `tokio::spawn` in `MistEngine::handle_message_content` once a session
    /// has joined the overlay mesh -- see `Peer::negotiating`'s doc comment
    /// for that dispatch shape), and blocking that task for the whole
    /// debounce window would needlessly delay whatever runs after this call
    /// returns. After the delay, re-checks that `self` still has a live
    /// session for `sender_id` -- a request that raced a teardown of that
    /// session during the debounce window has nothing left to repair, same
    /// as the original immediate check.
    fn handle_restart_request(&self, sender_id: NodeId) {
        let handles = self.peer_handles();
        tokio::spawn(async move {
            let jitter_ms = super::repair_trigger_jitter_ms(&handles.local_node_id, &sender_id);
            let delay_ms = super::REPAIR_TRIGGER_DEBOUNCE_MS + jitter_ms;
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;

            let has_peer = {
                let peers = handles.peers.read().await;
                peers.contains_key(&sender_id)
            };
            if !has_peer {
                tracing::debug!(
                    "[IceRestart] ignoring RestartRequest from {}: no existing session after the \
                     {}ms debounce (reconnection initiation stays with CONNECT_REQUEST/the \
                     overlay balancer)",
                    sender_id,
                    delay_ms
                );
                return;
            }
            handles
                .maybe_try_ice_restart(&sender_id, "restart_request")
                .await;
        });
    }
}

impl WebRtcTransport {
    pub(crate) async fn handle_offer(
        &self,
        remote_id: NodeId,
        sdp: String,
    ) -> crate::error::Result<()> {
        // An inbound offer means the remote is alive and negotiating with us,
        // so any pending CONNECT_REQUEST retry loop for this node is obsolete
        // regardless of which path below handles the offer.
        self.clear_connect_request_attempt(&remote_id);

        // If we already have a live peer for this remote, this offer is
        // (almost always) a renegotiation on the existing RTCPeerConnection
        // -- e.g. tc-chat's browser side calling `publish_local_track` to add
        // a screen-share track mid-session, which renegotiates with a fresh
        // SDP offer over the same signaling channel rather than opening a
        // new one. Apply it directly to the existing peer instead of falling
        // through to the brand-new-connection path below, which would
        // discard the live peer (`old_peer.close_all()`) and its already-open
        // data channels/tracks out from under an active session.
        let existing_peer = {
            let peers = self.peers.read().await;
            peers.get(&remote_id).cloned()
        };

        if let Some(peer) = existing_peer {
            // Glare resolution: if we're the impolite side (lower ID =
            // initiator) and we have our own offer in flight on this same
            // peer connection, ignore their offer so ours proceeds instead of
            // colliding. This only fires for a genuine collision (our own
            // offer is unanswered, signaling_state == HaveLocalOffer) --
            // legitimate renegotiation offers arriving once negotiation is
            // Stable are not glare and must not be dropped based on ID
            // ordering alone (that would permanently block renegotiation
            // from a peer whenever our local ID happens to sort lower).
            if self.local_node_id.0 < remote_id.0
                && peer.pc.signaling_state() == RTCSignalingState::HaveLocalOffer
            {
                tracing::debug!(
                    "[Glare] ignoring offer from {} (we are impolite side, our offer is in flight)",
                    remote_id
                );
                return Ok(());
            }

            // Remote-takeover fix, Change 2 -- see the doc comment on the
            // `impl WebRtcTransport` block above this function. Only when
            // the incoming offer's DTLS fingerprint differs from the
            // existing session's (and the takeover guards allow it) do we
            // tear the stale session down and fall through to the
            // brand-new-connection path below, exactly as if no existing
            // peer had been found. Otherwise this offer is applied to the
            // existing PC exactly as before (same fingerprint == legit
            // renegotiation/ICE restart; missing fingerprint == conservative
            // fallback; guard-rejected takeover == unchanged behavior).
            if self
                .should_takeover_on_fresh_offer(&remote_id, &peer, &sdp)
                .await
            {
                self.takeover_stale_session(&remote_id, "remote_new_offer_takeover")
                    .await;
            } else {
                // Offer resend, receiver side: a byte-identical re-delivery
                // of an offer already applied to this peer (the initiator's
                // `sweeper::spawn_offer_resend` retransmitting an
                // unanswered Offer) must be handled idempotently rather than
                // falling into `apply_offer`'s ordinary
                // renegotiation/glare path -- see
                // `duplicate_offer_decision`'s doc comment for the full
                // state table. A non-identical offer (the overwhelmingly
                // common case) always resolves to `None` here and falls
                // through unchanged.
                let remote_matches = peer
                    .pc
                    .remote_description()
                    .await
                    .is_some_and(|desc| desc.sdp == sdp);
                let local_desc = peer.pc.local_description().await;
                let local_is_answer = local_desc
                    .as_ref()
                    .is_some_and(|desc| desc.sdp_type == RTCSdpType::Answer);
                let decision = duplicate_offer_decision(
                    remote_matches,
                    peer.pc.signaling_state(),
                    local_is_answer,
                );
                match decision {
                    Some(DuplicateOfferAction::ResendAnswer) => {
                        tracing::info!(
                            "[OfferResend] duplicate offer from {}: re-sending existing answer",
                            remote_id
                        );
                        // `local_is_answer` only resolves `true` when
                        // `local_desc` is `Some`, so this is always
                        // populated here.
                        if let Some(answer_desc) = local_desc {
                            let msg = MessageContent::Data(SignalingData {
                                sender_id: self.local_node_id.clone(),
                                receiver_id: remote_id.clone(),
                                room_id: self.get_room_id(),
                                data: answer_desc.sdp,
                                signaling_type: SignalingType::Answer,
                            });
                            if let Err(err) = self.signaler.send_signaling(&remote_id, msg).await {
                                tracing::warn!(
                                    "[OfferResend] failed to re-send existing answer to {}: {:?}",
                                    remote_id,
                                    err
                                );
                            }
                        }
                        return Ok(());
                    }
                    Some(DuplicateOfferAction::Ignore) => {
                        tracing::debug!(
                            "[OfferResend] ignoring duplicate offer from {}: our answer to it is \
                             still being produced (HaveRemoteOffer)",
                            remote_id
                        );
                        return Ok(());
                    }
                    None => {}
                }
                return self.apply_offer(remote_id, sdp, peer).await;
            }
        }

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
                        matches!(
                            **s,
                            ConnectionState::Connected
                                | ConnectionState::Connecting
                                | ConnectionState::Reconnecting
                        )
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

        if newly_reserved {
            // Sweeper livelock fix: the answer side queues on
            // `acquire_handshake_permit` (below) exactly like the dial side
            // does in `connect_inner` -- see `connecting_reserved_at`'s doc
            // comment for the race this closes.
            self.connecting_reserved_at
                .write()
                .unwrap()
                .insert(remote_id.clone(), Instant::now());
        }

        self.acquire_handshake_permit(&remote_id).await?;

        if !self.has_active_session(&remote_id) {
            self.handshake_permits.write().unwrap().remove(&remote_id);
            return Ok(());
        }

        let peer = match self.create_pc(remote_id.clone()).await {
            Ok(p) => p,
            Err(e) => {
                self.handshake_permits.write().unwrap().remove(&remote_id);
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

        let old_peer = {
            let mut peers = self.peers.write().await;
            peers.insert(remote_id.clone(), peer.clone())
        };
        // Mirror the insert into `send_queues` -- see
        // `WebRtcTransport::send_queues`'s doc comment. Inserted after the
        // `peers` insert above, per its ordering note.
        self.send_queues
            .write()
            .unwrap()
            .insert(remote_id.clone(), peer.send_tx.clone());
        if let Some(old_peer) = old_peer {
            tracing::warn!(
                "[WebRTC Close] reason=handle_offer_replace_peer node={}",
                remote_id
            );
            old_peer.close_all().await;
            crate::mem::record_peer_cleaned();
        }
        crate::mem::record_peer_inserted();

        let attempt_id = self.reserve_connection_attempt(&remote_id);
        // `[ConnTiming]` instrumentation: attempt-start timestamp for the
        // inbound-offer (answer-side) path -- see
        // `WebRtcTransport::connect_started_at`'s doc comment.
        self.connect_started_at
            .write()
            .unwrap()
            .insert(remote_id.clone(), Instant::now());
        // `[ConnTiming]` instrumentation: the answering side's connection
        // attempt is starting right now, at the same point
        // `connect_started_at` is stamped.
        super::conn_timing::log_attempt_start(&remote_id);
        self.spawn_connection_watchdog(remote_id.clone(), attempt_id);

        let result = self.apply_offer(remote_id.clone(), sdp, peer.clone()).await;

        if result.is_err() {
            // Guard on the peer this call just inserted: a concurrent
            // `connect_inner` for the same `NodeId` racing this failed
            // answer attempt may have already installed its own, healthy
            // peer by the time we get here. An unconditional-by-NodeId
            // cleanup would delete that live registration instead of this
            // attempt's own (failed) one -- see
            // `PeerSharedHandles::cleanup_session_if_current`'s doc comment.
            let expected = Arc::downgrade(&peer);
            self.cleanup_session_if_current(&remote_id, &expected, true, "handle_offer_error")
                .await;
        } else if self.has_published_tracks() {
            // New-peer hook, answer-side completion: `create_pc` (called
            // above) already attached every published track to `peer`'s
            // RTCPeerConnection before `apply_offer` ran, but per JSEP the
            // answer we just sent cannot introduce m= sections beyond what
            // the remote's offer contained -- `webrtc-rs`'s `create_answer`
            // silently omits any local transceiver that doesn't match one in
            // the remote offer (`generate_matched_sdp(..., includeUnmatched:
            // false, ...)`). So the published tracks' transceivers exist on
            // the peer connection but are not yet negotiated. Once signaling
            // has settled back to Stable (which `apply_offer` just did), send
            // a follow-up offer of our own -- webrtc-rs's `create_offer` does
            // include unmatched local transceivers once a remote description
            // is already set, so this picks them up. A live network test of
            // this exact path needs real UDP ICE, which restricted networks
            // do not provide (see `tests/loopback_media.rs`); this is deliberately a
            // separate, best-effort step so a renegotiation failure here
            // doesn't undo the connection that `apply_offer` already
            // established.
            if let Err(err) = self.send_offer(&remote_id, &peer).await {
                tracing::warn!(
                    "failed to renegotiate published tracks with new peer {}: {:?}",
                    remote_id,
                    err
                );
            }
        }

        result
    }

    /// Applies an inbound offer to `peer`'s RTCPeerConnection and answers it:
    /// `set_remote_description` -> `create_answer` -> `set_local_description`
    /// -> send the answer back over signaling. Shared by both `handle_offer`
    /// call sites -- a brand-new peer (right after `create_pc`) and an
    /// existing live peer being renegotiated -- since the offer/answer
    /// mechanics are identical either way; only what happens on error differs
    /// (the caller decides whether to tear the peer down).
    async fn apply_offer(
        &self,
        remote_id: NodeId,
        sdp: String,
        peer: Arc<Peer>,
    ) -> crate::error::Result<()> {
        // Held for the whole set_remote_description -> create_answer ->
        // set_local_description -> send sequence below -- see
        // `Peer::negotiating`'s doc comment. Both of `handle_offer`'s call
        // sites read `signaling_state` before reaching here without holding
        // this lock, so re-check it immediately below now that we actually
        // hold it: a concurrent negotiation step on this same peer (e.g.
        // another `apply_offer` for a second offer that arrived moments
        // later via the overlay signaling path, which dispatches each
        // inbound message on its own unserialized `tokio::spawn`) may have
        // already changed the state by the time this call gets its turn.
        let _negotiating = peer.negotiating.lock().await;

        if peer.pc.signaling_state() != RTCSignalingState::Stable {
            return Err(crate::error::MistError::Internal(format!(
                "Offer precondition failed: signaling_state={:?}",
                peer.pc.signaling_state()
            )));
        }

        let offer = parse_offer_payload(&sdp)?;
        if let Err(e) = peer.pc.set_remote_description(offer).await {
            rollback_to_stable_on_failure(&peer.pc, &remote_id).await;
            return Err(e.into());
        }

        let answer = match peer.pc.create_answer(None).await {
            Ok(answer) => answer,
            Err(e) => {
                rollback_to_stable_on_failure(&peer.pc, &remote_id).await;
                return Err(e.into());
            }
        };
        if let Err(e) = peer.pc.set_local_description(answer).await {
            rollback_to_stable_on_failure(&peer.pc, &remote_id).await;
            return Err(e.into());
        }

        let cands = {
            let mut pc_lock = self.pending_candidates.write().await;
            pc_lock.remove(&remote_id)
        };
        self.pending_candidates_first_seen
            .write()
            .await
            .remove(&remote_id);

        if let Some(cands) = cands {
            for cand_json in cands {
                match serde_json::from_str::<RTCIceCandidateInit>(&cand_json) {
                    Ok(candidate) => {
                        if let Err(err) = peer.pc.add_ice_candidate(candidate).await {
                            tracing::warn!(
                                "failed to apply buffered ICE candidate for {}: {}",
                                remote_id.0,
                                err
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            "failed to parse buffered ICE candidate for {}: {}",
                            remote_id.0,
                            err
                        );
                    }
                }
            }
        }

        if let Some(answer_desc) = peer.pc.local_description().await {
            let data = answer_desc.sdp;
            if let Err(err) = self
                .signaler
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
            {
                // Answer-side hardening ride-along (offer-resend spec): this
                // used to propagate silently (`.map_err(...)?` with no log of
                // its own) -- log with the node id before returning so an
                // undelivered Answer is visible on its own, not only as a
                // downstream `handle_offer_error` cleanup with no indication
                // of which signaling step actually failed. No retry is added
                // here: the initiator's own Offer-resend
                // (`sweeper::spawn_offer_resend`) already drives a fresh
                // re-answer if the remote never received this one.
                tracing::warn!("failed to send answer to {}: {:?}", remote_id, err);
                return Err(crate::error::MistError::Core(err));
            }
        }
        Ok(())
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

            let answer = parse_answer_payload(&sdp)?;
            if let Err(e) = peer.pc.set_remote_description(answer).await {
                // A malformed/rejected answer would otherwise leave this peer
                // stuck at HaveLocalOffer forever (our own offer already
                // applied earlier by `send_offer`) -- see
                // `rollback_to_stable_on_failure`'s doc comment.
                rollback_to_stable_on_failure(&peer.pc, &remote_id).await;
                return Err(e.into());
            }

            let cands = {
                let mut pc_lock = self.pending_candidates.write().await;
                pc_lock.remove(&remote_id)
            };
            self.pending_candidates_first_seen
                .write()
                .await
                .remove(&remote_id);

            if let Some(cands) = cands {
                for cand_json in cands {
                    match serde_json::from_str::<RTCIceCandidateInit>(&cand_json) {
                        Ok(candidate) => {
                            if let Err(err) = peer.pc.add_ice_candidate(candidate).await {
                                tracing::warn!(
                                    "failed to apply buffered ICE candidate for {}: {}",
                                    remote_id.0,
                                    err
                                );
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                "failed to parse buffered ICE candidate for {}: {}",
                                remote_id.0,
                                err
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Handles an inbound trickled ICE candidate. If a live peer already has
    /// a remote description set, the candidate is applied immediately;
    /// otherwise it is buffered in `pending_candidates` for
    /// `apply_offer`/`handle_answer` to drain once that description is set.
    ///
    /// Buffer-don't-drop fix: this buffers regardless of whether `remote_id`
    /// currently has a `connection_states` reservation at all -- previously,
    /// a candidate for a node with no reservation was dropped outright on
    /// the theory that late candidates for an already-disconnected node
    /// would otherwise accumulate unboundedly. That theory missed a much
    /// more common case: inbound signaling messages are dispatched via
    /// independent, unordered per-message tasks (see
    /// `mistlib_core::engine::network` -- this module must not change that
    /// dispatch model), so there is no ordering guarantee between an Offer
    /// and a Candidate for the same node. A trickled Candidate scheduled
    /// before its Offer landed in exactly this "no reservation yet" branch
    /// and was silently lost -- surfacing later as "pingAllCandidates called
    /// with no candidate pairs" and a `watchdog_connect_timeout` against a
    /// peer that looked completely silent (measured 1623/30min on a steady
    /// 50-node fleet, no fault injection). Unbounded growth is now prevented
    /// two other ways instead: `MAX_PENDING_CANDIDATES_PER_NODE`'s existing
    /// per-node cap, and `MAX_PENDING_CANDIDATE_NODES`'s new total-node-count
    /// cap (a node that never materializes still eventually ages out via
    /// `pending_candidates_first_seen`'s sweep in `sweeper.rs`).
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

        let node_str = remote_id.0.clone();
        let mut pc_lock = self.pending_candidates.write().await;
        let is_new_node = !pc_lock.contains_key(&remote_id);
        if is_new_node && pc_lock.len() >= super::MAX_PENDING_CANDIDATE_NODES {
            drop(pc_lock);
            tracing::warn!(
                "refusing to buffer ICE candidate for new node {}: pending_candidates already \
                 tracks {} distinct nodes (MAX_PENDING_CANDIDATE_NODES)",
                node_str,
                super::MAX_PENDING_CANDIDATE_NODES
            );
            return Ok(());
        }
        let list = pc_lock.entry(remote_id.clone()).or_default();
        let dropped_oldest = super::push_pending_candidate(list, cand_json);
        drop(pc_lock);

        if is_new_node {
            self.pending_candidates_first_seen
                .write()
                .await
                .insert(remote_id, Instant::now());
        }

        if dropped_oldest {
            tracing::warn!(
                "pending ICE candidates for {} exceeded {}; dropped oldest",
                node_str,
                super::MAX_PENDING_CANDIDATES_PER_NODE
            );
        }
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
            SignalingType::Offer => {
                let envelope = serde_json::from_str::<NegotiationEnvelope>(&data.data).ok();
                let payload = envelope
                    .as_ref()
                    .map_or_else(|| data.data.clone(), |item| item.sdp.clone());
                self.handle_offer(data.sender_id.clone(), payload)
                    .await
                    .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;
                if let Some(envelope) = envelope {
                    self.send_negotiation_ack(&data.sender_id, &current_room_id, envelope.id)
                        .await;
                }
                Ok(())
            }
            SignalingType::Answer => {
                let envelope = serde_json::from_str::<NegotiationEnvelope>(&data.data).ok();
                let payload = envelope
                    .as_ref()
                    .map_or_else(|| data.data.clone(), |item| item.sdp.clone());
                self.handle_answer(data.sender_id.clone(), payload)
                    .await
                    .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;
                if let Some(envelope) = envelope {
                    self.send_negotiation_ack(&data.sender_id, &current_room_id, envelope.id)
                        .await;
                }
                Ok(())
            }
            SignalingType::Candidate => {
                let envelope = serde_json::from_str::<CandidateEnvelope>(&data.data).ok();
                let candidate = envelope
                    .as_ref()
                    .map_or_else(|| data.data.clone(), |item| item.candidate.clone());
                self.handle_candidate(data.sender_id.clone(), candidate)
                    .await
                    .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;

                if let Some((envelope, mask)) = envelope.and_then(|envelope| {
                    1_u64
                        .checked_shl(u32::from(envelope.sequence))
                        .map(|mask| (envelope, mask))
                }) {
                    let ack = CandidateAck {
                        generation: envelope.generation,
                        mask,
                    };
                    if let Ok(payload) = serde_json::to_string(&ack) {
                        let _ = self
                            .signaler
                            .send_signaling(
                                &data.sender_id,
                                MessageContent::Data(SignalingData {
                                    sender_id: self.local_node_id.clone(),
                                    receiver_id: data.sender_id.clone(),
                                    room_id: current_room_id.clone(),
                                    data: payload,
                                    signaling_type: SignalingType::CandidateAck,
                                }),
                            )
                            .await;
                    }
                }
                Ok(())
            }
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
                // Repair-first ICE restart, Change 3: a `Request` tagged with
                // `RESTART_REQUEST_MARKER` is a `RestartRequest` repair nudge,
                // not the ordinary CONNECT_REQUEST connection-initiation hint
                // -- see `RESTART_REQUEST_MARKER`'s doc comment (`webrtc.rs`)
                // for why this is layered on the existing `Request` message
                // instead of a new `SignalingType` variant. Handled entirely
                // separately from the CONNECT_REQUEST logic below: it must
                // never itself initiate a brand-new connection.
                if super::is_restart_request(&data.data) {
                    self.handle_restart_request(data.sender_id);
                    return Ok(());
                }

                let direct_to_local = data.receiver_id == self.local_node_id;
                let legacy_broadcast_tiebreak =
                    data.receiver_id.is_broadcast() && self.local_node_id.0 < data.sender_id.0;
                let deterministic_offerer = self.local_node_id.0 < data.sender_id.0;
                if self.local_node_id != data.sender_id
                    && deterministic_offerer
                    && (direct_to_local || legacy_broadcast_tiebreak)
                {
                    // Remote-takeover fix, Change 1 -- see the doc comment on
                    // the `impl WebRtcTransport` block above `handle_offer`.
                    // A no-op unless we still hold a stale peer entry for
                    // `data.sender_id` and the takeover guards allow it; in
                    // every other case behaves exactly as before.
                    self.maybe_takeover_for_connect_request(&data.sender_id)
                        .await;
                    let _ = self.connect(&data.sender_id).await;
                }
                Ok(())
            }
            // Native currently emits legacy raw candidates, so it has no
            // retry state to retire when an ACK arrives.
            SignalingType::CandidateAck => Ok(()),
            // Native still emits legacy one-shot Offer/Answer payloads.
            SignalingType::NegotiationAck => Ok(()),
            SignalingType::Rejoin => {
                // Locally-synthesized-only notification (see
                // `SignalingType::Rejoin`'s doc comment): the signaling layer
                // detected that `data.sender_id` rebound to a fresh signaling
                // identity, i.e. the peer restarted (browser reload / process
                // restart) while keeping the same host-assigned NodeId. An
                // abruptly-vanished WebRTC peer still reports
                // `readyState == Open` locally for tens of seconds (nothing
                // ever told this side the old session is dead), so without
                // this the peer's real Offer/Request that immediately
                // follows on the same ordered signaling stream would be
                // misapplied to (or ignored in favor of) the stale,
                // unrecoverable `RTCPeerConnection`.
                //
                // Reuses the same guarded teardown machinery as every other
                // "supersede a stale peer" path (`cleanup_session_if_current`,
                // used by the remote-takeover paths above, the connect
                // watchdog, and the periodic sweeper) rather than an
                // unconditional-by-NodeId removal: the snapshot-then-compare
                // shape means that if a fresh session for this exact NodeId
                // has already raced in ahead of this notification, this is a
                // no-op instead of destroying the new, live registration.
                let peer = {
                    let peers = self.peers.read().await;
                    peers.get(&data.sender_id).cloned()
                };
                if let Some(peer) = peer {
                    tracing::info!(
                        "WebRtcTransport: rejoin detected for {} (new session epoch={}); \
                         tearing down stale peer connection",
                        data.sender_id,
                        data.data
                    );
                    let expected = Arc::downgrade(&peer);
                    self.cleanup_session_if_current(
                        &data.sender_id,
                        &expected,
                        true,
                        "rejoin_detected",
                    )
                    .await;
                }
                Ok(())
            }
        }
    }
}

/// Remote-takeover fix, Change 2: extracts and normalizes the DTLS
/// fingerprint from an SDP's `a=fingerprint:<hash-algo> <hex>` line (RFC
/// 8122 via RFC 8842's `a=fingerprint` SDP attribute), if present. Used by
/// `should_takeover_on_fresh_offer` to tell whether an inbound offer was
/// produced by the SAME `RTCPeerConnection` (and therefore the same DTLS
/// certificate) as the one already on file for a peer, or by a fresh one.
///
/// Both the hash algorithm name and the hex digest are case-insensitive per
/// RFC 8122 5.1.3 (implementations MUST treat "sha-256"/"SHA-256" and
/// mixed-case hex as equivalent); webrtc-rs and browsers are consistent
/// about emitting lowercase hash-algorithm names with colon-separated
/// uppercase hex, but nothing guarantees every peer this process ever talks
/// to will format it identically, so this normalizes both parts (hash
/// algorithm lowercased, hex uppercased, colons left as-is) before
/// returning -- two fingerprints that are semantically identical but
/// differently cased must compare equal. The algorithm name is kept as part
/// of the returned value (not discarded) so a hypothetical algorithm change
/// between two otherwise-identical-looking sessions is never mistaken for
/// "same certificate".
///
/// Returns `None` if no `a=fingerprint:` line is present, or if a present
/// line is malformed (missing the hex half) -- both cases are treated
/// identically by callers as "fingerprint unavailable", the conservative
/// "keep today's behavior" case in `offer_takeover_decision`.
///
/// Pure (no I/O, no transport state) so every shape is exhaustively
/// unit-testable -- see `webrtc/tests/takeover.rs`.
pub(crate) fn sdp_fingerprint(sdp: &str) -> Option<String> {
    for line in sdp.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("a=fingerprint:") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let algo = parts.next()?;
        let hex = parts.next()?;
        return Some(format!(
            "{} {}",
            algo.to_ascii_lowercase(),
            hex.to_ascii_uppercase()
        ));
    }
    None
}

/// Remote-takeover fix, Change 2: the three-way decision `handle_offer`
/// makes when an existing peer is found for an inbound offer's sender --
/// compares the existing session's current remote-description fingerprint
/// against the incoming offer's, and only when they differ does it apply the
/// shared guards (`super::takeover_allowed`, see its doc comment in
/// `webrtc.rs`) that Change 1 also uses. Returns `true` only when the stale
/// session should be torn down and this offer treated as a brand-new
/// connection attempt:
///
/// - Both fingerprints present and equal -> `false` (same remote PC, i.e. a
///   legitimate renegotiation or an ICE restart from the peer's own
///   `try_ice_restart`, which reuses its PC and therefore its cert -- keep
///   applying to the existing PC).
/// - Both present and different -> the guarded takeover decision
///   (`takeover_allowed`).
/// - Either missing (no remote description set yet -- e.g. the existing
///   session is still `Connecting` -- or no fingerprint line in the SDP) ->
///   `false`, conservatively: keep applying to the existing PC rather than
///   risk tearing down a session we can't actually prove is stale.
///
/// Pure so the whole fingerprint x guard decision table is unit-testable
/// without a live handshake -- see `webrtc/tests/takeover.rs`.
/// Offer resend, receiver side: what `handle_offer`'s existing-peer path
/// should do about an inbound offer that byte-identically re-delivers one
/// already applied to this peer -- see `duplicate_offer_decision`'s doc
/// comment for the decision table this backs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DuplicateOfferAction {
    /// The remote missed our answer to this exact offer -- re-send it
    /// verbatim. `set_remote_description` must NOT be called again: the
    /// remote description already on file is (by construction of this
    /// variant) identical to the incoming SDP, so nothing about negotiation
    /// state needs to change.
    ResendAnswer,
    /// Our answer to this exact offer is still being produced
    /// (`HaveRemoteOffer`, a sub-ms window between `set_remote_description`
    /// and the in-flight `apply_offer` call sending its answer) -- the
    /// duplicate is covered by that in-flight call and needs no action here.
    Ignore,
}

/// Offer resend, receiver side: pure decision backing `handle_offer`'s
/// existing-peer path. `remote_matches` is whether the incoming SDP is
/// byte-identical to `peer.pc.remote_description()`'s SDP; `signaling_state`
/// and `local_is_answer` are the peer's current negotiation state and
/// whether its `local_description()` (if any) is an `Answer`-typed
/// description.
///
/// Only resolves an action when `remote_matches` -- a non-identical offer
/// (any real renegotiation, or a takeover already handled by the caller
/// before this is ever reached) always falls through unchanged to
/// `apply_offer`, `None` here. Among byte-identical re-deliveries:
///
/// - `Stable` + a local `Answer` on file -> [`DuplicateOfferAction::ResendAnswer`]:
///   negotiation for this exact offer already completed and settled back to
///   `Stable`; the remote re-sent because it never got our answer, so only
///   the answer needs re-sending, not the negotiation itself.
/// - `HaveRemoteOffer` -> [`DuplicateOfferAction::Ignore`]: the very first
///   delivery of this same offer is still being answered by another,
///   concurrent `apply_offer` call on this same peer (see `Peer::negotiating`'s
///   doc comment for how two inbound messages for one peer can race in the
///   first place); that in-flight call will send the answer once it
///   completes.
/// - Any other `signaling_state` (e.g. `HaveLocalOffer` -- already handled by
///   `handle_offer`'s earlier glare check before this is ever reached, or
///   `Stable` with no local `Answer` on file, e.g. we are the initiator of
///   this session) -> `None`, conservatively falling through to
///   `apply_offer`'s ordinary precondition check rather than guessing.
///
/// Pure so the whole table is exhaustively unit-testable without a live
/// handshake -- see `webrtc/tests/signaling.rs`.
pub(crate) fn duplicate_offer_decision(
    remote_matches: bool,
    signaling_state: RTCSignalingState,
    local_is_answer: bool,
) -> Option<DuplicateOfferAction> {
    if !remote_matches {
        return None;
    }
    match signaling_state {
        RTCSignalingState::Stable if local_is_answer => Some(DuplicateOfferAction::ResendAnswer),
        RTCSignalingState::HaveRemoteOffer => Some(DuplicateOfferAction::Ignore),
        _ => None,
    }
}

pub(crate) fn offer_takeover_decision(
    existing_fingerprint: Option<&str>,
    incoming_fingerprint: Option<&str>,
    healthy: bool,
    ms_since_connected: Option<u64>,
    ms_since_last_takeover: Option<u64>,
    ms_since_connect_started: Option<u128>,
) -> bool {
    match (existing_fingerprint, incoming_fingerprint) {
        (Some(existing), Some(incoming)) if existing != incoming => super::takeover_allowed(
            healthy,
            ms_since_connected,
            ms_since_last_takeover,
            ms_since_connect_started,
        ),
        _ => false,
    }
}

pub(crate) fn parse_offer_payload(payload: &str) -> crate::error::Result<RTCSessionDescription> {
    if let Ok(description) = serde_json::from_str::<RTCSessionDescription>(payload) {
        return Ok(description);
    }
    RTCSessionDescription::offer(payload.to_string()).map_err(Into::into)
}

pub(crate) fn parse_answer_payload(payload: &str) -> crate::error::Result<RTCSessionDescription> {
    if let Ok(description) = serde_json::from_str::<RTCSessionDescription>(payload) {
        return Ok(description);
    }
    RTCSessionDescription::answer(payload.to_string()).map_err(Into::into)
}

#[cfg(test)]
mod payload_tests {
    use super::*;

    const MINIMAL_SDP: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";

    #[test]
    fn parse_offer_payload_accepts_raw_sdp() {
        let parsed = parse_offer_payload(MINIMAL_SDP).unwrap();
        assert_eq!(parsed.sdp, MINIMAL_SDP);
    }

    #[test]
    fn parse_offer_payload_accepts_legacy_json_description() {
        let json = serde_json::json!({ "type": "offer", "sdp": MINIMAL_SDP }).to_string();
        let parsed = parse_offer_payload(&json).unwrap();
        assert_eq!(parsed.sdp, MINIMAL_SDP);
    }

    #[test]
    fn parse_answer_payload_accepts_raw_sdp() {
        let parsed = parse_answer_payload(MINIMAL_SDP).unwrap();
        assert_eq!(parsed.sdp, MINIMAL_SDP);
    }
}
