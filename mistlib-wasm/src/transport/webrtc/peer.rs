use super::candidate_delivery::{
    CandidateDelivery, TrackStatus, CANDIDATE_RETRY_DELAYS_MS, MAX_TRACKED_CANDIDATES_PER_NODE,
};
use super::negotiation_delivery::NegotiationDelivery;
use super::send_queue::SendQueue;
use super::{DisconnectGrace, GraceOrigin};
use mistlib_core::signaling::{
    CandidateEnvelope, MessageContent, Signaler, SignalingData, SignalingType,
};
use mistlib_core::stats::STATS;
use mistlib_core::transport::{NetworkEvent, NetworkEventHandler};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    MediaStream, MessageEvent, RtcDataChannel, RtcPeerConnection, RtcPeerConnectionIceEvent,
    RtcTrackEvent,
};

async fn send_candidate_once(
    signaler: Arc<dyn Signaler>,
    local_id: NodeId,
    remote_id: NodeId,
    room_id: String,
    payload: String,
) {
    if let Err(error) = signaler
        .send_signaling(
            &remote_id,
            MessageContent::Data(SignalingData {
                sender_id: local_id,
                receiver_id: remote_id.clone(),
                room_id,
                data: payload,
                signaling_type: SignalingType::Candidate,
            }),
        )
        .await
    {
        tracing::warn!("Failed to send ICE candidate to {}: {}", remote_id.0, error);
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_candidate_with_retries(
    signaler: Arc<dyn Signaler>,
    local_id: NodeId,
    remote_id: NodeId,
    room_id: String,
    payload: String,
    delivery: Arc<RwLock<CandidateDelivery>>,
    generation: u32,
    sequence: u8,
    tracked: bool,
) {
    let sends = if tracked {
        CANDIDATE_RETRY_DELAYS_MS.len() + 1
    } else {
        1
    };
    for attempt in 0..sends {
        if attempt > 0 {
            gloo_timers::future::TimeoutFuture::new(CANDIDATE_RETRY_DELAYS_MS[attempt - 1]).await;
            if !delivery
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&remote_id, generation, sequence)
            {
                return;
            }
        }
        let result = signaler
            .send_signaling(
                &remote_id,
                MessageContent::Data(SignalingData {
                    sender_id: local_id.clone(),
                    receiver_id: remote_id.clone(),
                    room_id: room_id.clone(),
                    data: payload.clone(),
                    signaling_type: SignalingType::Candidate,
                }),
            )
            .await;
        if attempt > 0 {
            tracing::warn!(
                "Retrying ICE candidate to {} generation={} sequence={} attempt={} result={}",
                remote_id.0,
                generation,
                sequence,
                attempt + 1,
                if result.is_ok() { "sent" } else { "failed" }
            );
        } else if let Err(error) = result {
            tracing::warn!(
                "Failed to send ICE candidate to {} generation={} sequence={}: {}",
                remote_id.0,
                generation,
                sequence,
                error
            );
        }
    }

    if tracked
        && delivery
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .expire(&remote_id, generation, sequence)
    {
        tracing::warn!(
            "ICE candidate ACK exhausted for {} generation={} sequence={}",
            remote_id.0,
            generation,
            sequence
        );
    }
}

pub struct Peer {
    pub pc: RtcPeerConnection,
    pub channels: Arc<RwLock<HashMap<DeliveryMethod, RtcDataChannel>>>,
    /// Waiters for `WasmWebRtcTransport::send`'s backpressure wait (see
    /// `wait_for_buffered_amount_low` in `webrtc.rs`), keyed by
    /// `DeliveryMethod` rather than by DataChannel instance -- a peer has at
    /// most one channel per method at a time. `setup_dc_handlers` registers a
    /// single *permanent* `onbufferedamountlow` handler per channel that
    /// drains this list on every fire, instead of each waiting `send()` call
    /// swapping in its own `set_onbufferedamountlow` closure and clobbering
    /// any other call already waiting on the same channel (which would leave
    /// the clobbered waiter to time out even though the channel drained).
    pub buffered_amount_waiters:
        Arc<RwLock<HashMap<DeliveryMethod, Vec<tokio::sync::oneshot::Sender<()>>>>>,
    /// Serializes every operation that advances this peer's
    /// `RTCPeerConnection` signaling state via `createOffer`/`createAnswer`
    /// + `setLocalDescription`/`setRemoteDescription` --
    /// `WasmWebRtcTransport::renegotiate_peer`, `apply_offer_in_place`, and
    /// `trigger_ice_restart` all take this lock for their full
    /// create-then-apply sequence. Without it, two of those triggered back
    /// to back on the same connected peer (e.g. `publish_local_track` called
    /// twice in a row for a screen-share's video and audio tracks, neither
    /// awaited before the next fires) race: both read `Stable` and both call
    /// `createOffer`, but only one's `setLocalDescription` can apply first --
    /// the other's captured offer no longer matches the connection's actual
    /// state by the time it lands, and Chrome rejects it with
    /// `InvalidModificationError: SDP is modified in a non-acceptable way`.
    /// Held across the whole create+apply(+send) sequence rather than just
    /// the state check, so the second caller genuinely waits its turn
    /// instead of re-reading a state snapshot that's already stale by the
    /// time it acts on it.
    pub negotiating: tokio::sync::Mutex<()>,
    /// Set when a track publish/unpublish changed this peer's senders but the
    /// follow-up renegotiation could not run (peer in ICE-disconnected
    /// recovery grace, transient non-`Stable` signaling, ...) -- see
    /// `WasmWebRtcTransport::publish_local_track`/`unpublish_local_track` and
    /// the answer-side new-peer follow-up in `handle_offer`. Instead of
    /// surfacing such a transient condition as a hard publish error (and
    /// permanently losing the track for this peer, since nothing used to
    /// retry), the deferral is recorded here and `setup_handlers`'s
    /// ICE-Connected/Completed arm triggers
    /// `WasmWebRtcTransport::reconcile_peer_tracks` once the peer actually
    /// recovers. Peers torn down instead of recovering don't need it: a
    /// re-handshake builds a fresh `Peer` whose `create_pc` attaches every
    /// published track before first negotiation.
    pub needs_track_reconcile: std::sync::atomic::AtomicBool,
    /// Bounded FIFO of `ReliableOrdered` sends deferred by
    /// `WasmWebRtcTransport::send` while this peer exists but isn't
    /// `Connected` yet/still (fresh connection pre-`onopen`, or mid
    /// ICE-restart grace -- see `send_queue::should_queue_reliable_send`).
    /// Lives on `Peer` itself (rather than a separate `NodeId`-keyed map on
    /// the transport) so it's torn down for free whenever the peer is:
    /// dropping the `Arc<Peer>` drops this too, with no separate cleanup
    /// bookkeeping to keep in sync with every peer-removal path. `close_all`
    /// and the ICE `Failed`/`Closed` arm in `setup_handlers` additionally
    /// drain it explicitly with a `warn!` on non-empty teardown, rather than
    /// relying on a silent `Drop`.
    pub send_queue: Mutex<SendQueue>,
}

impl Peer {
    pub fn new(pc: RtcPeerConnection) -> Self {
        Self {
            pc,
            channels: Arc::new(RwLock::new(HashMap::new())),
            buffered_amount_waiters: Arc::new(RwLock::new(HashMap::new())),
            negotiating: tokio::sync::Mutex::new(()),
            needs_track_reconcile: std::sync::atomic::AtomicBool::new(false),
            send_queue: Mutex::new(SendQueue::default()),
        }
    }

    /// Whether at least one DataChannel is `Open` right now. Shared by the
    /// ICE-recovery state-repair decision
    /// (`super::recovery::state_after_ice_recovery`, see `setup_handlers`'s
    /// `oniceconnectionstatechange` handler below) and
    /// `WasmWebRtcTransport::request_action_for`'s Request-signaling dedup --
    /// both used to carry their own inline copy of this exact check.
    ///
    /// Deliberately checks `Open` only, unlike the sweeper's and the
    /// connection watchdog's own inline checks in `webrtc.rs`, which also
    /// accept `Connecting` -- those two are asking a different question
    /// ("is there a channel still alive/establishing", for timeout/liveness
    /// purposes) than this one ("is there a channel actually usable for
    /// `send()` right now"), so they are intentionally left as their own
    /// inline checks rather than folded into this helper.
    pub fn has_open_channel(&self) -> bool {
        let channels = self.channels.read().unwrap_or_else(|e| e.into_inner());
        channels
            .values()
            .any(|dc| dc.ready_state() == web_sys::RtcDataChannelState::Open)
    }

    /// Drops everything queued in `send_queue` without replaying it,
    /// returning how many messages were dropped so the caller can `warn!`
    /// (with node-id context it has in scope but `Peer` itself doesn't) if it
    /// wasn't already empty -- mirrors `PendingCandidates::push`'s
    /// `dropped_oldest` bool /  `buffer_candidate_if_active`'s caller-side
    /// log in `webrtc.rs`. Called from `close_all` (below) and from every
    /// other path that removes this peer for good; see the `send_queue`
    /// field doc for why it lives here instead of a separately-tracked map.
    pub fn clear_send_queue(&self) -> usize {
        let mut queue = self.send_queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.clear()
    }

    /// Replays every message queued in `send_queue` onto the
    /// `ReliableOrdered` DataChannel, in FIFO order -- but only if that
    /// channel is actually `Open` right now. Called from two places, both
    /// with no lock of the caller's own held at the call site (per the WASM
    /// no-reentrant-mutex constraint, see the comment on the `onmessage`
    /// handler below): the DC `onopen` handler (this same file) once the
    /// `ReliableOrdered` channel itself opens, and the ICE `Connected`/
    /// `Completed` arm once `super::recovery::state_after_ice_recovery` says
    /// the repaired state is `Connected` (spawned via `spawn_local` there
    /// rather than called inline, since that arm's caller is a synchronous
    /// event handler still holding its own lock at the point it decides to
    /// flush).
    ///
    /// If the channel isn't there or isn't `Open` (shouldn't normally happen
    /// -- both call sites only fire once the reliable channel is actually
    /// usable -- but this is defensive rather than assumed-safe), this is a
    /// no-op and the queue is left untouched for a later flush attempt.
    pub fn flush_send_queue(&self, node: &NodeId) {
        let dc = {
            let channels = self.channels.read().unwrap_or_else(|e| e.into_inner());
            channels.get(&DeliveryMethod::ReliableOrdered).cloned()
        };
        let Some(dc) = dc else { return };
        if dc.ready_state() != web_sys::RtcDataChannelState::Open {
            return;
        }

        let messages = {
            let mut queue = self.send_queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.drain()
        };
        if messages.is_empty() {
            return;
        }

        tracing::debug!(
            "Flushing {} queued reliable message(s) to {}",
            messages.len(),
            node.0
        );
        for data in messages {
            if let Err(err) = dc.send_with_u8_array(&data) {
                tracing::warn!(
                    "Failed to flush a queued reliable message to {}: {:?}",
                    node.0,
                    err
                );
                continue;
            }
            STATS.add_send(data.len() as u64);
            STATS.add_world_send_frame(&data);
        }
    }

    pub fn close_all(&self, node: &NodeId) {
        self.detach_peer_handlers();

        let channels = {
            let mut lock = self.channels.write().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *lock)
        };

        for (_, dc) in channels {
            Self::detach_data_channel_handlers(&dc);
            dc.close();
        }

        // Wakes any in-flight `wait_for_buffered_amount_low` callers right
        // away instead of leaving them to sit out the full timeout: dropping
        // their `oneshot::Sender` here surfaces as a recv error, which
        // `wait_for_buffered_amount_low` treats the same as a timeout (drop
        // the message) -- correct either way since the channel is closing.
        self.buffered_amount_waiters
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        self.pc.close();

        let dropped = self.clear_send_queue();
        if dropped > 0 {
            tracing::warn!(
                "Dropping {} queued reliable message(s) for {} on peer teardown",
                dropped,
                node.0
            );
        }
    }

    fn detach_peer_handlers(&self) {
        self.pc.set_oniceconnectionstatechange(None);
        self.pc.set_onicecandidate(None);
        self.pc.set_ondatachannel(None);
        self.pc.set_ontrack(None);
    }

    fn detach_data_channel_handlers(dc: &RtcDataChannel) {
        dc.set_onopen(None);
        dc.set_onclose(None);
        dc.set_onmessage(None);
        dc.set_onbufferedamountlow(None);
    }

    pub fn setup_handlers(
        self: &Arc<Self>,
        remote_id: NodeId,
        signaler: Arc<dyn Signaler>,
        local_id: NodeId,
        room_id: String,
        connection_states: Arc<RwLock<HashMap<NodeId, ConnectionState>>>,
        disconnected_since: Arc<RwLock<HashMap<NodeId, DisconnectGrace>>>,
        event_handler: Arc<Mutex<Option<Arc<dyn NetworkEventHandler>>>>,
        peers: Arc<RwLock<HashMap<NodeId, Arc<Peer>>>>,
        peer_senders: Arc<RwLock<HashMap<NodeId, HashMap<String, web_sys::RtcRtpSender>>>>,
        pending_candidates: Arc<RwLock<crate::transport::webrtc::PendingCandidates>>,
        candidate_delivery: Arc<RwLock<CandidateDelivery>>,
        negotiation_delivery: Arc<RwLock<NegotiationDelivery>>,
        candidate_generation: u32,
    ) {
        let conn_states = connection_states.clone();
        let remote_id_state = remote_id.clone();
        let peer_state = self.clone();
        let peers_state = peers.clone();
        let disconnected_since_state = disconnected_since.clone();
        let senders_state = peer_senders.clone();
        let pending_state = pending_candidates.clone();
        let candidate_delivery_state = candidate_delivery.clone();
        let negotiation_delivery_state = negotiation_delivery.clone();
        let room_id_state = room_id.clone();
        let signaler_ice_restart = signaler.clone();
        let local_id_ice_restart = local_id.clone();
        let negotiation_delivery_ice_restart = negotiation_delivery.clone();
        let onstatechange = Closure::wrap(Box::new(move |_ev: web_sys::Event| {
            let state = peer_state.pc.ice_connection_state();
            let is_current = {
                let peers = peers_state.read().unwrap_or_else(|e| e.into_inner());
                peers
                    .get(&remote_id_state)
                    .is_some_and(|current| Arc::ptr_eq(current, &peer_state))
            };
            if !is_current {
                tracing::debug!(
                    "Ignoring stale ICE state callback from {} ({:?})",
                    remote_id_state.0,
                    state
                );
                return;
            }
            tracing::info!(
                "ICE Connection state to {} changed to: {:?}",
                remote_id_state.0,
                state
            );
            let mut states = conn_states.write().unwrap_or_else(|e| e.into_inner());
            match state {
                web_sys::RtcIceConnectionState::Connected
                | web_sys::RtcIceConnectionState::Completed => {
                    // Decide the repaired state up front, before anything
                    // else in this arm, so this insertion stays
                    // position-independent with respect to whatever else
                    // this arm may also do on recovery (e.g. a possible
                    // deferred-renegotiation trigger). Replaces the old
                    // unconditional `ConnectionState::Connecting` -- see
                    // `recovery::state_after_ice_recovery`'s doc comment for
                    // the confirmed bug this fixes: after an ICE restart,
                    // the existing (still-open) DataChannels never re-fire
                    // `onopen`, so unconditionally demoting to `Connecting`
                    // here left the peer stuck failing every `send()` with
                    // "Not connected" forever.
                    let trigger = if state == web_sys::RtcIceConnectionState::Connected {
                        super::recovery::IceRecoveryTrigger::Connected
                    } else {
                        super::recovery::IceRecoveryTrigger::Completed
                    };
                    let has_open_channel = peer_state.has_open_channel();
                    let new_state =
                        super::recovery::state_after_ice_recovery(trigger, has_open_channel);
                    states.insert(remote_id_state.clone(), new_state);

                    let recovered = disconnected_since_state
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&remote_id_state)
                        .is_some();
                    if recovered {
                        tracing::info!(
                            "ICE recovered for {} during disconnected grace (state repaired to {:?}).",
                            remote_id_state.0,
                            new_state
                        );
                    }

                    // Flush any `ReliableOrdered` sends deferred while this
                    // peer wasn't `Connected` (see `send_queue` /
                    // `WasmWebRtcTransport::send`). Spawned rather than
                    // called inline: this whole closure is a synchronous
                    // browser event handler and `states` above is still
                    // locked at this point in the arm -- `spawn_local`'s
                    // future only actually runs after this closure returns
                    // (and drops the lock), same reasoning as any other
                    // deferred-work spawn in this handler. Deliberately does
                    // NOT call `crate::app::emit_peer_connected` here even
                    // though `new_state` may be `Connected`: see
                    // `state_after_ice_recovery`'s doc comment for why the
                    // app already considers this peer connected in that
                    // case -- only the DC `onopen` path's own
                    // `prev != Connected` check decides whether to emit.
                    if new_state == ConnectionState::Connected {
                        let peer_flush = peer_state.clone();
                        let remote_id_flush = remote_id_state.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            peer_flush.flush_send_queue(&remote_id_flush);
                        });
                    }
                    // A publish/unpublish that hit this peer while it was in
                    // a transient state (e.g. the ICE-disconnected grace we
                    // may just have recovered from) changed the senders but
                    // deferred its renegotiation (see
                    // `Peer::needs_track_reconcile`). Now that ICE is back,
                    // run that renegotiation so the track change actually
                    // reaches the remote instead of being lost forever. Looked
                    // up via the session registry (`crate::app`) because this
                    // closure only captures individual maps, not the owning
                    // `WasmWebRtcTransport` -- same pattern as the
                    // `emit_peer_disconnected` call in the Failed arm below.
                    // Spawned, not awaited: this is a sync event handler, and
                    // the spawned future takes `Peer::negotiating` internally.
                    if peer_state
                        .needs_track_reconcile
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        let remote_id = remote_id_state.clone();
                        let room_id = room_id_state.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            let Some(transport) = crate::app::session_webrtc(&room_id) else {
                                return;
                            };
                            transport.reconcile_peer_tracks(&remote_id).await;
                        });
                    }
                }
                web_sys::RtcIceConnectionState::Disconnected => {
                    states.insert(remote_id_state.clone(), ConnectionState::Reconnecting);
                    // `Entry` (rather than `or_insert_with`'s return value, which
                    // doesn't say whether it inserted) tells us whether this is
                    // the transition that *started* the grace period, as
                    // opposed to a state flicker while one is already running --
                    // the ICE restart below fires at most once per grace period.
                    let is_new_grace = match disconnected_since_state
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .entry(remote_id_state.clone())
                    {
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(DisconnectGrace {
                                started_at: web_time::Instant::now(),
                                origin: GraceOrigin::Ice,
                            });
                            true
                        }
                        std::collections::hash_map::Entry::Occupied(_) => false,
                    };
                    tracing::warn!(
                        "ICE disconnected for {}. keeping peer for recovery grace.",
                        remote_id_state.0
                    );

                    // Only the initiator (lower NodeId) restarts, so both
                    // sides don't race to send competing restart offers; the
                    // other side's restart offer arrives as a normal Offer
                    // and is applied in-place (offer_guard::OfferAction::ApplyInPlace).
                    let is_initiator = local_id_ice_restart.0 < remote_id_state.0;
                    let signaling_is_stable =
                        peer_state.pc.signaling_state() == web_sys::RtcSignalingState::Stable;
                    if super::ice_restart::should_trigger_ice_restart(
                        is_new_grace,
                        is_initiator,
                        signaling_is_stable,
                    ) {
                        let restart_peer = peer_state.clone();
                        let signaler = signaler_ice_restart.clone();
                        let local_id = local_id_ice_restart.clone();
                        let remote_id = remote_id_state.clone();
                        let room_id = room_id_state.clone();
                        let negotiation_delivery = negotiation_delivery_ice_restart.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            super::trigger_ice_restart(
                                restart_peer,
                                signaler,
                                local_id,
                                remote_id,
                                room_id,
                                negotiation_delivery,
                            )
                            .await;
                        });
                    }
                }
                web_sys::RtcIceConnectionState::Failed | web_sys::RtcIceConnectionState::Closed => {
                    states.insert(remote_id_state.clone(), ConnectionState::Disconnected);
                    {
                        let mut disconnected = disconnected_since_state
                            .write()
                            .unwrap_or_else(|e| e.into_inner());
                        disconnected.remove(&remote_id_state);
                    }
                    {
                        let mut peers = peers_state.write().unwrap_or_else(|e| e.into_inner());
                        peers.remove(&remote_id_state);
                    }
                    {
                        let mut senders = senders_state.write().unwrap_or_else(|e| e.into_inner());
                        senders.remove(&remote_id_state);
                    }
                    {
                        let mut pending = pending_state.write().unwrap_or_else(|e| e.into_inner());
                        pending.remove(&remote_id_state);
                    }
                    {
                        candidate_delivery_state
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove_node(&remote_id_state);
                        negotiation_delivery_state
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove_node(&remote_id_state);
                    }
                    // Every other path that removes a peer for good routes
                    // through `Peer::close_all`, which drains `send_queue`
                    // itself -- this arm is the one exception (the ICE
                    // transport is already Failed/Closed, so there's nothing
                    // left worth calling `.close()` on). `peer_state` is
                    // this closure's own clone of the same `Arc<Peer>` just
                    // removed from `peers_state` above, so clear its queue
                    // explicitly here instead of relying on an eventual
                    // `Drop` to free it silently.
                    let dropped = peer_state.clear_send_queue();
                    if dropped > 0 {
                        tracing::warn!(
                            "Dropping {} queued reliable message(s) for {} (ICE {:?})",
                            dropped,
                            remote_id_state.0,
                            state
                        );
                    }
                    // The Sweeper skips isolation-recovery when the peer is already
                    // gone from the map, so we trigger it here directly.
                    crate::app::emit_peer_disconnected(
                        remote_id_state.clone(),
                        room_id_state.clone(),
                    );
                }
                web_sys::RtcIceConnectionState::Checking => {
                    states.insert(remote_id_state.clone(), ConnectionState::Connecting);
                }
                _ => {}
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        self.pc
            .set_oniceconnectionstatechange(Some(onstatechange.as_ref().unchecked_ref()));
        onstatechange.forget();

        let signaler_cb = signaler.clone();
        let local_id_cb = local_id.clone();
        let remote_id_cand = remote_id.clone();
        let room_id_cb = room_id.clone();
        let candidate_delivery_cb = candidate_delivery.clone();
        let next_candidate_sequence = Arc::new(AtomicU32::new(0));

        let onicecandidate = Closure::wrap(Box::new(move |ev: RtcPeerConnectionIceEvent| {
            if let Some(candidate) = ev.candidate() {
                let signaler = signaler_cb.clone();
                let local_id = local_id_cb.clone();
                let remote_id = remote_id_cand.clone();
                let room_id = room_id_cb.clone();
                let delivery = candidate_delivery_cb.clone();
                let sequence = next_candidate_sequence.fetch_add(1, Ordering::Relaxed);
                wasm_bindgen_futures::spawn_local(async move {
                    let cand_json = candidate.to_json();
                    let cand_str = js_sys::JSON::stringify(&cand_json)
                        .unwrap_or_default()
                        .as_string()
                        .unwrap_or_default();

                    if sequence >= MAX_TRACKED_CANDIDATES_PER_NODE as u32 {
                        tracing::warn!(
                            "ICE candidate count exceeded bitmap capacity for {} generation={}; sending once",
                            remote_id.0,
                            candidate_generation
                        );
                        send_candidate_once(signaler, local_id, remote_id, room_id, cand_str).await;
                        return;
                    }

                    let sequence = sequence as u8;
                    let envelope = CandidateEnvelope {
                        generation: candidate_generation,
                        sequence,
                        candidate: cand_str,
                    };
                    let Ok(payload) = serde_json::to_string(&envelope) else {
                        return;
                    };
                    let tracked = matches!(
                        delivery.write().unwrap_or_else(|e| e.into_inner()).track(
                            remote_id.clone(),
                            candidate_generation,
                            sequence
                        ),
                        TrackStatus::New
                    );
                    send_candidate_with_retries(
                        signaler,
                        local_id,
                        remote_id,
                        room_id,
                        payload,
                        delivery,
                        candidate_generation,
                        sequence,
                        tracked,
                    )
                    .await;
                });
            }
        }) as Box<dyn FnMut(RtcPeerConnectionIceEvent)>);
        self.pc
            .set_onicecandidate(Some(onicecandidate.as_ref().unchecked_ref()));
        onicecandidate.forget();

        let peer_dc = self.clone();
        let event_handler_dc = event_handler.clone();
        let remote_id_dc = remote_id.clone();
        let connection_states_dc = connection_states.clone();
        let peers_dc = peers.clone();
        let peer_senders_dc = peer_senders.clone();
        let room_id_dc = room_id.clone();
        let ondatachannel = Closure::wrap(Box::new(move |ev: web_sys::RtcDataChannelEvent| {
            let dc = ev.channel();
            let label = dc.label();
            let method = match label.as_str() {
                "reliable" => DeliveryMethod::ReliableOrdered,
                "unreliable-ordered" => DeliveryMethod::UnreliableOrdered,
                "unreliable" => DeliveryMethod::Unreliable,
                _ => DeliveryMethod::ReliableOrdered,
            };
            {
                let mut channels = peer_dc.channels.write().unwrap_or_else(|e| e.into_inner());
                channels.insert(method, dc.clone());
            }
            Peer::setup_dc_handlers(
                dc,
                method,
                peer_dc.clone(),
                event_handler_dc.clone(),
                remote_id_dc.clone(),
                room_id_dc.clone(),
                connection_states_dc.clone(),
                disconnected_since.clone(),
                peers_dc.clone(),
                peer_senders_dc.clone(),
                pending_candidates.clone(),
            );
        })
            as Box<dyn FnMut(web_sys::RtcDataChannelEvent)>);
        self.pc
            .set_ondatachannel(Some(ondatachannel.as_ref().unchecked_ref()));
        ondatachannel.forget();

        let remote_id_track = remote_id.clone();
        let ontrack = Closure::wrap(Box::new(move |ev: RtcTrackEvent| {
            let track = ev.track();
            let track_id = track.id();
            let kind = track.kind();
            let streams = ev.streams();
            let stream = streams.get(0).dyn_into::<MediaStream>().ok();

            crate::app::emit_media_track_added(
                remote_id_track.clone(),
                track_id.clone(),
                kind.clone(),
                track.clone(),
                stream,
            );

            let remote_id_ended = remote_id_track.clone();
            let track_id_ended = track_id.clone();
            let kind_ended = kind.clone();
            let onended = Closure::wrap(Box::new(move |_ev: web_sys::Event| {
                crate::app::emit_media_track_removed(
                    remote_id_ended.clone(),
                    track_id_ended.clone(),
                    kind_ended.clone(),
                );
            }) as Box<dyn FnMut(web_sys::Event)>);
            track.set_onended(Some(onended.as_ref().unchecked_ref()));
            onended.forget();
        }) as Box<dyn FnMut(RtcTrackEvent)>);
        self.pc.set_ontrack(Some(ontrack.as_ref().unchecked_ref()));
        ontrack.forget();
    }

    pub fn setup_dc_handlers(
        dc: RtcDataChannel,
        method: DeliveryMethod,
        peer: Arc<Peer>,
        handler: Arc<Mutex<Option<Arc<dyn NetworkEventHandler>>>>,
        from: NodeId,
        room_id: String,
        connection_states: Arc<RwLock<HashMap<NodeId, ConnectionState>>>,
        disconnected_since: Arc<RwLock<HashMap<NodeId, DisconnectGrace>>>,
        peers: Arc<RwLock<HashMap<NodeId, Arc<Peer>>>>,
        peer_senders: Arc<RwLock<HashMap<NodeId, HashMap<String, web_sys::RtcRtpSender>>>>,
        pending_candidates: Arc<RwLock<crate::transport::webrtc::PendingCandidates>>,
    ) {
        // Takes the whole `Arc<Peer>` (rather than just its
        // `buffered_amount_waiters`, as before) so `onopen` below can flush
        // `peer.send_queue` directly once the ReliableOrdered channel opens,
        // without a fallible lookup back through `peers` by `from` -- which
        // could in principle resolve to a *different*, newer `Peer` if a
        // reconnect had already replaced the map entry by the time `onopen`
        // fires. Capturing the exact `Peer` this channel belongs to is
        // always correct.
        let buffered_amount_waiters = peer.buffered_amount_waiters.clone();
        dc.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);
        // Lets the browser fire `onbufferedamountlow` once a congested
        // channel's outbound queue drains back down to this -- shared setup
        // point for both the offering side (created in `connect`) and the
        // answering side (received via `ondatachannel` below), so this one
        // call covers every channel. See `WasmWebRtcTransport::send`'s
        // backpressure handling.
        dc.set_buffered_amount_low_threshold(super::BUFFERED_AMOUNT_LOW_THRESHOLD);

        // Registered exactly once per channel (never swapped out) and kept
        // for the channel's whole lifetime: fires drain every waiter queued
        // for `method` in `wait_for_buffered_amount_low`. This is the fix for
        // the clobbering bug a per-wait `set_onbufferedamountlow` had -- two
        // concurrent Reliable sends waiting on the same channel would
        // otherwise have the second wait's handler silently replace the
        // first's, leaving the first waiting for an event it will never see.
        let waiters_low = buffered_amount_waiters.clone();
        let onbufferedamountlow = Closure::wrap(Box::new(move |_ev: web_sys::Event| {
            let drained: Vec<tokio::sync::oneshot::Sender<()>> = {
                let mut lock = waiters_low.write().unwrap_or_else(|e| e.into_inner());
                lock.remove(&method).unwrap_or_default()
            };
            for tx in drained {
                // Err means the waiter already timed out and dropped its
                // receiver; nothing to do.
                let _ = tx.send(());
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        dc.set_onbufferedamountlow(Some(onbufferedamountlow.as_ref().unchecked_ref()));
        onbufferedamountlow.forget();

        let label = dc.label().to_string();
        let from_msg = from.clone();
        let states_open = connection_states.clone();
        let disconnected_open = disconnected_since.clone();
        let room_id_open = room_id.clone();
        let peer_open = peer.clone();
        let peers_open = peers.clone();
        let onopen = Closure::wrap(Box::new(move |_ev: web_sys::Event| {
            tracing::info!("DataChannel {} to {} opened", label, from_msg.0);
            // ReliableOrdered is the transport's required control/data path.
            // Marking the peer Connected when either best-effort channel opens
            // can suppress the connection watchdog while the required channel
            // is still unusable.
            if method != DeliveryMethod::ReliableOrdered {
                return;
            }
            let is_current = {
                let lock = peers_open.read().unwrap_or_else(|e| e.into_inner());
                lock.get(&from_msg)
                    .is_some_and(|current| Arc::ptr_eq(current, &peer_open))
            };
            if !is_current {
                tracing::debug!(
                    "Ignoring stale DataChannel open callback from {}",
                    from_msg.0
                );
                return;
            }

            let prev = {
                let mut lock = states_open.write().unwrap_or_else(|e| e.into_inner());
                lock.insert(from_msg.clone(), ConnectionState::Connected)
            };
            disconnected_open
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&from_msg);

            if prev != Some(ConnectionState::Connected) {
                crate::app::emit_peer_connected(from_msg.clone(), room_id_open.clone());
            }
            peer_open.flush_send_queue(&from_msg);
        }) as Box<dyn FnMut(web_sys::Event)>);
        dc.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();

        let from_close = from.clone();
        let from_close_for_cleanup = from.clone();
        let states_close = connection_states.clone();
        let disconnected_close = disconnected_since.clone();
        let peers_close = peers.clone();
        let peer_close_identity = peer.clone();
        let senders_close = peer_senders.clone();
        let pending_close = pending_candidates.clone();
        let room_id_close = room_id.clone();
        let onclose = Closure::wrap(Box::new(move |_ev: web_sys::Event| {
            tracing::warn!(
                "DataChannel to {} closed, triggering immediate disconnect.",
                from_close.0
            );

            let peer = {
                let mut lock = peers_close.write().unwrap_or_else(|e| e.into_inner());
                let is_current = lock
                    .get(&from_close_for_cleanup)
                    .is_some_and(|current| Arc::ptr_eq(current, &peer_close_identity));
                if is_current {
                    lock.remove(&from_close_for_cleanup)
                } else {
                    None
                }
            };

            let Some(peer) = peer else { return };

            {
                let mut lock = states_close.write().unwrap_or_else(|e| e.into_inner());
                lock.remove(&from_close);
            }
            {
                let mut lock = disconnected_close
                    .write()
                    .unwrap_or_else(|e| e.into_inner());
                lock.remove(&from_close);
            }
            {
                let mut lock = senders_close.write().unwrap_or_else(|e| e.into_inner());
                lock.remove(&from_close_for_cleanup);
            }
            {
                let mut lock = pending_close.write().unwrap_or_else(|e| e.into_inner());
                lock.remove(&from_close_for_cleanup);
            }
            peer.close_all(&from_close);
            // peerが確実に1回だけ取り出せた場合のみ切断通知を送る（重複防止済み）
            crate::app::emit_peer_disconnected(from_close.clone(), room_id_close.clone());
        }) as Box<dyn FnMut(web_sys::Event)>);
        dc.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();

        let onmessage = Closure::wrap(Box::new(move |ev: MessageEvent| {
            if let Ok(ab) = ev.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = js_sys::Uint8Array::new(&ab);
                let vec = array.to_vec();
                STATS.add_receive(vec.len() as u64);
                STATS.add_world_receive_frame(&vec);
                // Clone the Arc out of the mutex in a short-lived scope so the
                // lock is released before on_event is called.  Calling on_event
                // while still holding the lock can trigger re-entrant callbacks
                // that try to acquire the same mutex, causing the WASM
                // no_threads mutex to panic with "cannot recursively acquire
                // mutex".
                let maybe_h = {
                    let lock = handler.lock().unwrap_or_else(|e| e.into_inner());
                    lock.as_ref().cloned()
                };
                if let Some(h) = maybe_h {
                    h.on_event(NetworkEvent {
                        from: from.clone(),
                        data: bytes::Bytes::from(vec),
                    });
                } else {
                    tracing::warn!(
                        "DataChannel message from {} dropped - no handler registered",
                        from.0
                    );
                }
            } else {
                tracing::warn!(
                    "Received DataChannel message from {} but it's not an ArrayBuffer",
                    from.0
                );
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        dc.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();
    }
}
