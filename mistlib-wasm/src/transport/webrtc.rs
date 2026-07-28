use async_trait::async_trait;
use bytes::Bytes;
use js_sys::Reflect;
use mistlib_core::config::IceServer;
use mistlib_core::signaling::{
    MessageContent, Signaler, SignalingData, SignalingHandler, SignalingType,
};
use mistlib_core::stats::STATS;
use mistlib_core::transport::{NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MediaStreamTrack, RtcConfiguration, RtcDataChannelInit, RtcIceCandidateInit,
    RtcIceConnectionState, RtcOfferOptions, RtcPeerConnection, RtcRtpSender, RtcSdpType,
    RtcSessionDescriptionInit, RtcSignalingState,
};
use web_time::Instant;

const CONNECTION_TIMEOUT_MS: u32 = 6000;
const ISOLATION_RECOVERY_DELAY_MS: u32 = 3000;
#[cfg(test)]
const DISCONNECTED_GRACE_MS: u64 = 50;
#[cfg(not(test))]
const DISCONNECTED_GRACE_MS: u64 = 5000;

/// What triggered a peer's current reconnect-grace period. `clear_suspect` may
/// only cancel a `LivenessSuspect`-origin grace: an `Ice`-origin one is left
/// alone for ICE's own recovery signal to end (see
/// `WasmWebRtcTransport::clear_suspect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraceOrigin {
    Ice,
    LivenessSuspect,
}

/// A single entry in `disconnected_since`: when the grace period started, and
/// what triggered it.
#[derive(Debug, Clone, Copy)]
pub struct DisconnectGrace {
    pub started_at: Instant,
    pub origin: GraceOrigin,
}

/// Whether `cleanup_peer_connection` should arm `schedule_isolation_recovery`
/// after tearing a peer down. `Schedule` (the historical, always-on behavior)
/// is correct for a genuine teardown with nothing queued up to replace it --
/// `disconnect()`, the session sweeper, failure rollbacks, the watchdog's own
/// cleanup. `Skip` is for the restart-driven call sites where the very next
/// statement rebuilds the connection (`RequestAction::CleanupAndConnect` ->
/// `connect()`, `OfferAction::ReplacePeer` -> create-from-scratch, and the
/// `SignalingType::Rejoin` arm, whose real Offer/Request follows immediately
/// on the same ordered signaling stream): arming isolation recovery there
/// used to fire `schedule_isolation_recovery` on a peer that is *not*
/// actually isolated, just mid-rebuild, and its remedy
/// (`signaler.reset_session()`) rotates our own signaling identity -- which
/// the remote sees as another restart and answers with its own `Rejoin`,
/// tearing down the reconnect we were about to complete. That round-trip is
/// exactly the self-sustaining livelock this type exists to break; if a
/// future change "restores" an unconditional call here, the livelock comes
/// back. See also `is_isolated`'s in-flight-attempt check, which closes the
/// same hole from the read side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationRecovery {
    Schedule,
    Skip,
}
pub mod backpressure;
pub mod ice_config;
pub mod ice_restart;
pub mod isolation;
mod media;
pub mod message_guard;
pub mod offer_guard;
pub mod peer;
pub mod pending_candidates;
pub mod recovery;
pub mod request_guard;
pub mod sdp_lines;
pub mod send_queue;
use backpressure::{backpressure_action, BackpressureAction};
use ice_config::{build_ice_server_plans, ice_server_plans_to_js};
use isolation::is_isolated;
use message_guard::{check_message_size, SizeCheck};
use offer_guard::{
    active_connection_count, create_failure_rollback, offer_action_for_snapshot, OfferAction,
    OfferCreateFailureRollback, SignalingSnapshot,
};
pub use peer::Peer;
pub use pending_candidates::PendingCandidates;
use pending_candidates::{is_active_for_pending, MAX_PENDING_CANDIDATES_PER_NODE};
pub use request_guard::RequestAction;
use request_guard::{request_action_for_snapshot, RequestState};
use sdp_lines::mline_signature;
use send_queue::{should_queue_reliable_send, MAX_QUEUED_BYTES, MAX_QUEUED_MESSAGES};

/// Backpressure watermarks for a DataChannel's `bufferedAmount`
/// (`RTCDataChannel::send()` is fire-and-forget and never surfaces this on
/// its own -- see `backpressure` module and `WasmWebRtcTransport::send`).
/// `BUFFERED_AMOUNT_LOW_THRESHOLD` is set on every channel in
/// `Peer::setup_dc_handlers` so the browser fires `onbufferedamountlow` once
/// the queue drains back down to it.
const BUFFERED_AMOUNT_HIGH_WATERMARK: u32 = 1024 * 1024; // 1 MiB
const BUFFERED_AMOUNT_LOW_THRESHOLD: u32 = 256 * 1024; // 256 KiB
/// Upper bound on how long a Reliable send waits for `onbufferedamountlow`
/// before giving up and dropping the message.
const BUFFERED_AMOUNT_WAIT_TIMEOUT_MS: u32 = 3000;

#[derive(Clone)]
pub struct LocalTrack {
    pub track: MediaStreamTrack,
    pub kind: String,
    pub published: bool,
}

pub struct WasmWebRtcTransport {
    pub signaler: Arc<dyn Signaler>,
    pub local_node_id: NodeId,
    pub peers: Arc<RwLock<HashMap<NodeId, Arc<Peer>>>>,
    pub event_handler: Arc<Mutex<Option<Arc<dyn NetworkEventHandler>>>>,
    pub connection_states: Arc<RwLock<HashMap<NodeId, ConnectionState>>>,
    pub connection_attempt_ids: Arc<RwLock<HashMap<NodeId, u32>>>,
    pub pending_candidates: Arc<RwLock<PendingCandidates>>,
    pub disconnected_since: Arc<RwLock<HashMap<NodeId, DisconnectGrace>>>,
    pub room_id: Arc<RwLock<String>>,
    pub ice_servers: RwLock<Vec<IceServer>>,
    pub max_connections: AtomicU32,
    /// Mirrors `config.limits.max_message_bytes` (SPEC-13), set once from
    /// `build_session` the same way `max_connections`/`ice_servers` are --
    /// see `set_max_message_bytes`.
    pub max_message_bytes: AtomicU32,
    pub next_connection_attempt_id: AtomicU32,
    pub sweeper_started: AtomicBool,
    pub sweeper_generation: Arc<AtomicU32>,
    pub isolation_recovery_epoch: Arc<AtomicU32>,
    pub local_tracks: Arc<RwLock<HashMap<String, LocalTrack>>>,
    pub peer_senders: Arc<RwLock<HashMap<NodeId, HashMap<String, RtcRtpSender>>>>,
    /// One-shot latch recording that a peer's signaling identity rebound to a
    /// fresh session (browser reload, process restart) without ever cleanly
    /// closing the old `RTCPeerConnection` -- see `SignalingType::Rejoin`'s
    /// doc and the `Rejoin` arm of `SignalingHandler::handle_message`, which
    /// populates this. Keyed by the peer's `NodeId`, valued by the newest
    /// restart epoch observed (the restarted peer's `joined_at`, decimal unix
    /// millis, "0" if unknown) -- a later `Rejoin` for the same peer just
    /// overwrites the epoch rather than stacking.
    ///
    /// Consumed (removed) via `take_remote_restarted` the first time
    /// `request_action_for`/`handle_offer` reads it while deciding a
    /// `RequestAction`/`OfferAction` for that peer, so a restart forces
    /// exactly one teardown-and-rebuild rather than permanently marking the
    /// peer. Also cleared alongside the other per-peer maps wherever they are
    /// (`cleanup_peer_connection`, the session sweeper's per-node cleanup,
    /// `close_all_peer_connections`) so a peer that never sends a follow-up
    /// Request/Offer doesn't leave a dangling entry behind.
    pub restarted_peers: Arc<RwLock<HashMap<NodeId, u64>>>,
    /// Records the session epoch (the restarted peer's `joined_at`, same
    /// units as `restarted_peers`) that the *currently held* `Peer` for a
    /// node was built in response to -- written once a peer connection is
    /// (re)built as a result of consuming `restarted_peers` (in
    /// `RequestAction::CleanupAndConnect`'s `connect()` call and
    /// `OfferAction::ReplacePeer`'s accept path), read at the top of the
    /// `SignalingType::Rejoin` arm before any teardown happens.
    ///
    /// Exists because `restarted_peers` alone is one-shot and consumed by the
    /// *next* Request/Offer, not by the `Rejoin` that produced it -- so
    /// nothing previously recorded which epoch the live peer actually
    /// belongs to. A duplicate or reordered-late `Rejoin` carrying an epoch
    /// we've already rebuilt for would otherwise tear down a connection that
    /// is already the newest session, which is its own (smaller) instance of
    /// the isolation-recovery livelock this restart mechanism can trigger --
    /// see `IsolationRecovery`'s doc for the larger one. Cleared alongside
    /// `restarted_peers` everywhere that map is cleared, since it is the same
    /// per-peer, restart-scoped bookkeeping.
    pub peer_epochs: Arc<RwLock<HashMap<NodeId, u64>>>,
}

// --- Construction & configuration ---
impl WasmWebRtcTransport {
    pub fn new(signaler: Arc<dyn Signaler>, local_node_id: NodeId) -> Self {
        Self {
            signaler,
            local_node_id,
            peers: Arc::new(RwLock::new(HashMap::new())),
            event_handler: Arc::new(Mutex::new(None)),
            connection_states: Arc::new(RwLock::new(HashMap::new())),
            connection_attempt_ids: Arc::new(RwLock::new(HashMap::new())),
            pending_candidates: Arc::new(RwLock::new(PendingCandidates::default())),
            disconnected_since: Arc::new(RwLock::new(HashMap::new())),
            room_id: Arc::new(RwLock::new("lobby".to_string())),
            // Mirrors `Config::new_default()`'s `webrtc.ice_servers` (a single
            // Google STUN entry), same as `max_connections` below defaulting
            // to that config's `limits.max_connection_count`. `build_session`
            // overwrites this via `set_ice_servers` with the actual config,
            // so this default only matters for callers that construct the
            // transport directly without going through it.
            ice_servers: RwLock::new(vec![IceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_string()],
                username: None,
                credential: None,
            }]),
            max_connections: AtomicU32::new(30),
            // Mirrors `Config::new_default()`'s `limits.max_message_bytes`;
            // `build_session` overwrites this via `set_max_message_bytes`,
            // same caveat as `ice_servers`/`max_connections` above.
            max_message_bytes: AtomicU32::new(65536),
            next_connection_attempt_id: AtomicU32::new(1),
            sweeper_started: AtomicBool::new(false),
            sweeper_generation: Arc::new(AtomicU32::new(0)),
            isolation_recovery_epoch: Arc::new(AtomicU32::new(0)),
            local_tracks: Arc::new(RwLock::new(HashMap::new())),
            peer_senders: Arc::new(RwLock::new(HashMap::new())),
            restarted_peers: Arc::new(RwLock::new(HashMap::new())),
            peer_epochs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn ensure_session_sweeper(&self) {
        if self.sweeper_started.swap(true, Ordering::SeqCst) {
            return;
        }

        let generation = self
            .sweeper_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let peers = self.peers.clone();
        let states = self.connection_states.clone();
        let attempts = self.connection_attempt_ids.clone();
        let pending_candidates = self.pending_candidates.clone();
        let disconnected_since = self.disconnected_since.clone();
        let senders = self.peer_senders.clone();
        let restarted_peers = self.restarted_peers.clone();
        let peer_epochs = self.peer_epochs.clone();
        let sweeper_generation = self.sweeper_generation.clone();
        let signaler = self.signaler.clone();
        let isolation_recovery_epoch = self.isolation_recovery_epoch.clone();

        wasm_bindgen_futures::spawn_local(async move {
            loop {
                gloo_timers::future::TimeoutFuture::new(2000).await;
                if sweeper_generation.load(Ordering::Relaxed) != generation {
                    break;
                }

                let nodes: Vec<NodeId> = {
                    let lock = states.read().unwrap_or_else(|e| e.into_inner());
                    lock.keys().cloned().collect()
                };

                for node in nodes {
                    let peer_opt = {
                        let lock = peers.read().unwrap_or_else(|e| e.into_inner());
                        lock.get(&node).cloned()
                    };

                    let Some(peer) = peer_opt else {
                        let mut lock = states.write().unwrap_or_else(|e| e.into_inner());
                        lock.remove(&node);
                        let mut disconnected = disconnected_since
                            .write()
                            .unwrap_or_else(|e| e.into_inner());
                        disconnected.remove(&node);
                        pending_candidates
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&node);
                        restarted_peers
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&node);
                        peer_epochs
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&node);
                        continue;
                    };

                    let state_snapshot = {
                        let lock = states.read().unwrap_or_else(|e| e.into_inner());
                        lock.get(&node)
                            .copied()
                            .unwrap_or(ConnectionState::Disconnected)
                    };

                    let ice_state = peer.pc.ice_connection_state();
                    let has_open_channel = {
                        let channels = peer.channels.read().unwrap_or_else(|e| e.into_inner());
                        channels.values().any(|dc| {
                            matches!(
                                dc.ready_state(),
                                web_sys::RtcDataChannelState::Open
                                    | web_sys::RtcDataChannelState::Connecting
                            )
                        })
                    };

                    let failed_or_closed = matches!(
                        ice_state,
                        web_sys::RtcIceConnectionState::Failed
                            | web_sys::RtcIceConnectionState::Closed
                    );
                    let disconnected_grace_expired =
                        (matches!(ice_state, web_sys::RtcIceConnectionState::Disconnected)
                            || state_snapshot == ConnectionState::Reconnecting)
                            && disconnected_since
                                .read()
                                .unwrap_or_else(|e| e.into_inner())
                                .get(&node)
                                .is_some_and(|grace| {
                                    grace.started_at.elapsed().as_millis()
                                        >= DISCONNECTED_GRACE_MS as u128
                                });
                    let missing_channel =
                        state_snapshot == ConnectionState::Connected && !has_open_channel;

                    if failed_or_closed || disconnected_grace_expired || missing_channel {
                        if disconnected_grace_expired {
                            tracing::warn!("[Sweeper] disconnected grace expired for {}", node.0);
                        }
                        {
                            let mut lock = attempts.write().unwrap_or_else(|e| e.into_inner());
                            lock.remove(&node);
                        }
                        {
                            let mut lock = disconnected_since
                                .write()
                                .unwrap_or_else(|e| e.into_inner());
                            lock.remove(&node);
                        }
                        {
                            let mut lock = pending_candidates
                                .write()
                                .unwrap_or_else(|e| e.into_inner());
                            lock.remove(&node);
                        }
                        {
                            let mut lock = states.write().unwrap_or_else(|e| e.into_inner());
                            lock.insert(node.clone(), ConnectionState::Disconnected);
                        }
                        {
                            let mut lock = peers.write().unwrap_or_else(|e| e.into_inner());
                            lock.remove(&node);
                        }
                        {
                            let mut lock = senders.write().unwrap_or_else(|e| e.into_inner());
                            lock.remove(&node);
                        }
                        {
                            let mut lock =
                                restarted_peers.write().unwrap_or_else(|e| e.into_inner());
                            lock.remove(&node);
                        }
                        {
                            let mut lock = peer_epochs.write().unwrap_or_else(|e| e.into_inner());
                            lock.remove(&node);
                        }
                        peer.close_all(&node);
                        schedule_isolation_recovery(
                            signaler.clone(),
                            states.clone(),
                            attempts.clone(),
                            isolation_recovery_epoch.clone(),
                        );
                        tracing::warn!("[Sweeper] Force cleaned session for {}", node.0);
                    }
                }
            }
        });
    }

    pub fn stop_session_sweeper(&self) {
        self.sweeper_started.store(false, Ordering::SeqCst);
        self.sweeper_generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_room_id(&self, room_id: String) {
        let mut lock = self.room_id.write().unwrap_or_else(|e| e.into_inner());
        *lock = room_id;
    }

    pub fn set_max_connections(&self, max: u32) {
        self.max_connections.store(max, Ordering::Relaxed);
    }

    /// Sets the SPEC-13 payload size cap enforced by `Transport::send`.
    /// Called once from `build_session` with `config.limits.max_message_bytes`
    /// -- not live-reactive to a later `set_config()` mid-session, same as
    /// `set_max_connections`/`set_ice_servers`.
    pub fn set_max_message_bytes(&self, max: u32) {
        self.max_message_bytes.store(max, Ordering::Relaxed);
    }

    /// Sets the ICE (STUN/TURN) servers used by every peer connection created
    /// afterwards via `create_pc`. Called once from `build_session` with
    /// `config.webrtc.ice_servers`, mirroring `set_max_connections` above --
    /// neither is live-reactive to a later `set_config()` mid-session.
    pub fn set_ice_servers(&self, servers: Vec<IceServer>) {
        let mut lock = self.ice_servers.write().unwrap_or_else(|e| e.into_inner());
        *lock = servers;
    }
}

// --- ICE candidate handling ---
impl WasmWebRtcTransport {
    pub(crate) fn schedule_isolation_recovery(&self) {
        schedule_isolation_recovery(
            self.signaler.clone(),
            self.connection_states.clone(),
            self.connection_attempt_ids.clone(),
            self.isolation_recovery_epoch.clone(),
        );
    }

    fn apply_pending_candidates(&self, node: &NodeId, peer: &Peer) {
        let candidates = {
            let mut pending = self
                .pending_candidates
                .write()
                .unwrap_or_else(|e| e.into_inner());
            pending.take(node)
        };

        if let Some(candidates) = candidates {
            for cand_json in candidates {
                if let Err(err) = parse_and_add_candidate(node, peer, &cand_json) {
                    tracing::warn!(
                        "failed to apply pending ICE candidate for {}: {:?}",
                        node.0,
                        err
                    );
                }
            }
        }
    }

    fn buffer_candidate_if_active(&self, node: &NodeId, cand_json: String) {
        let should_buffer = {
            let states = self
                .connection_states
                .read()
                .unwrap_or_else(|e| e.into_inner());
            is_active_for_pending(states.get(node))
        };

        if !should_buffer {
            tracing::debug!(
                "dropping ICE candidate for {} without active connection state",
                node.0
            );
            return;
        }

        let dropped_oldest = {
            let mut pending = self
                .pending_candidates
                .write()
                .unwrap_or_else(|e| e.into_inner());
            pending.push(node.clone(), cand_json)
        };

        if dropped_oldest {
            tracing::warn!(
                "pending ICE candidates for {} exceeded {}; dropped oldest",
                node.0,
                MAX_PENDING_CANDIDATES_PER_NODE
            );
        }
    }

    fn handle_candidate_payload(&self, node: &NodeId, cand_json: String) {
        let peer = {
            let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
            peers.get(node).cloned()
        };

        if let Some(peer) = peer {
            if peer.pc.remote_description().is_some() {
                if let Err(err) = parse_and_add_candidate(node, &peer, &cand_json) {
                    tracing::warn!("failed to add ICE candidate for {}: {:?}", node.0, err);
                }
                return;
            }
        }

        self.buffer_candidate_if_active(node, cand_json);
    }

    fn create_pc(&self, remote_id: NodeId) -> Result<Arc<Peer>, JsValue> {
        let config = RtcConfiguration::new();
        let plans = {
            let servers = self.ice_servers.read().unwrap_or_else(|e| e.into_inner());
            build_ice_server_plans(&servers)
        };
        config.set_ice_servers(&ice_server_plans_to_js(&plans));

        let pc = RtcPeerConnection::new_with_configuration(&config)?;
        let peer = Arc::new(Peer::new(pc));

        peer.setup_handlers(
            remote_id.clone(),
            self.signaler.clone(),
            self.local_node_id.clone(),
            self.room_id
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            self.connection_states.clone(),
            self.disconnected_since.clone(),
            self.event_handler.clone(),
            self.peers.clone(),
            self.peer_senders.clone(),
            self.pending_candidates.clone(),
        );

        let _ = self.attach_published_tracks_to_peer(&remote_id, &peer)?;

        Ok(peer)
    }
}

// --- Peer connection lifecycle ---
impl WasmWebRtcTransport {
    pub async fn request_peers(&self) -> mistlib_core::error::Result<()> {
        let room_id = self
            .room_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.signaler
            .send_signaling(
                &NodeId("server".to_string()),
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

    fn cleanup_peer_connection(
        &self,
        node: &NodeId,
        close_pc: bool,
        isolation_recovery: IsolationRecovery,
    ) {
        tracing::info!(
            "cleanup_peer_connection: node={}, close_pc={}, isolation_recovery={:?}",
            node.0,
            close_pc,
            isolation_recovery
        );
        let removed_peer = {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            peers.remove(node)
        };

        if close_pc {
            if let Some(peer) = removed_peer {
                peer.close_all(node);
            }
        }

        {
            let mut senders = self.peer_senders.write().unwrap_or_else(|e| e.into_inner());
            senders.remove(node);
        }

        {
            let mut states = self
                .connection_states
                .write()
                .unwrap_or_else(|e| e.into_inner());
            states.insert(node.clone(), ConnectionState::Disconnected);
        }

        {
            let mut attempts = self
                .connection_attempt_ids
                .write()
                .unwrap_or_else(|e| e.into_inner());
            attempts.remove(node);
        }

        {
            let mut pending = self
                .pending_candidates
                .write()
                .unwrap_or_else(|e| e.into_inner());
            pending.remove(node);
        }

        // Matches every other per-peer map cleared above: usually already a
        // no-op here (both call sites that route through a restart --
        // `RequestAction::CleanupAndConnect` and `OfferAction::ReplacePeer`
        // -- already consumed this via `take_remote_restarted` before
        // reaching here), but this also runs from cleanup paths that have
        // nothing to do with a restart (`disconnect()`, failure rollbacks),
        // so a `Rejoin` that raced in without a follow-up Request/Offer
        // having been processed yet shouldn't outlive the peer it was about.
        {
            let mut restarted = self
                .restarted_peers
                .write()
                .unwrap_or_else(|e| e.into_inner());
            restarted.remove(node);
        }

        // Same per-peer, restart-scoped bookkeeping as `restarted_peers`
        // above -- see `peer_epochs`' field doc.
        {
            let mut epochs = self.peer_epochs.write().unwrap_or_else(|e| e.into_inner());
            epochs.remove(node);
        }

        // Fix for the isolation-recovery livelock: only genuine teardowns
        // (no reconnect queued up right after this call) should arm
        // `schedule_isolation_recovery`. See `IsolationRecovery`'s doc for
        // why an unconditional call here is actively harmful -- do not
        // "simplify" this back to always scheduling.
        if isolation_recovery == IsolationRecovery::Schedule {
            self.schedule_isolation_recovery();
        }
    }

    /// Reads-and-clears the one-shot `restarted_peers` latch for `node` (see
    /// its field doc). Called from exactly the two places that act on a
    /// peer's next Request or Offer -- `request_action_for` and
    /// `handle_offer` -- so the flag is consumed precisely when it's used to
    /// decide that message's `RequestAction`/`OfferAction`, not left to
    /// linger for some later, unrelated read.
    fn take_remote_restarted(&self, node: &NodeId) -> Option<u64> {
        self.restarted_peers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(node)
    }

    /// Returns the `RequestAction` to take for a Request from `node`,
    /// together with the restart epoch consumed from `restarted_peers` (if
    /// any). The epoch is handed back rather than swallowed here so the
    /// caller can stamp `peer_epochs` once the resulting reconnect
    /// (`RequestAction::CleanupAndConnect` -> `connect()`) actually succeeds
    /// -- see `peer_epochs`' field doc.
    fn request_action_for(&self, node: &NodeId) -> (RequestAction, Option<u64>) {
        let state = {
            let states = self
                .connection_states
                .read()
                .unwrap_or_else(|e| e.into_inner());
            states.get(node).copied()
        };
        let peer = {
            let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
            peers.get(node).cloned()
        };
        // Dedupes the old inline `channels.read()...any(ready_state == Open)`
        // check that used to live here into `Peer::has_open_channel`, also
        // shared by the ICE-recovery state-repair decision (see
        // `Peer::setup_handlers`'s `oniceconnectionstatechange` handler).
        let has_open_data_channel = peer.as_ref().is_some_and(|peer| peer.has_open_channel());
        let has_attempt = {
            let attempts = self
                .connection_attempt_ids
                .read()
                .unwrap_or_else(|e| e.into_inner());
            attempts.contains_key(node)
        };
        // See `restarted_peers`' field doc: consumed here, not just read, so
        // this Request is the one-shot "next Request or Offer" that clears
        // the latch.
        let remote_restarted_epoch = self.take_remote_restarted(node);

        let action = request_action_for_snapshot(RequestState {
            state,
            peer_exists: peer.is_some(),
            has_open_data_channel,
            has_attempt,
            remote_restarted: remote_restarted_epoch.is_some(),
        });
        (action, remote_restarted_epoch)
    }

    fn reserve_connection_attempt(&self, node: &NodeId) -> u32 {
        let attempt_id = self
            .next_connection_attempt_id
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        {
            let mut attempts = self
                .connection_attempt_ids
                .write()
                .unwrap_or_else(|e| e.into_inner());
            attempts.insert(node.clone(), attempt_id);
        }
        attempt_id
    }

    fn spawn_connection_watchdog(&self, node: NodeId, attempt_id: u32, peer: Arc<Peer>) {
        let conn_states = self.connection_states.clone();
        let attempt_ids = self.connection_attempt_ids.clone();
        let peers = self.peers.clone();
        let senders = self.peer_senders.clone();
        let pending_candidates = self.pending_candidates.clone();
        let signaler = self.signaler.clone();
        let isolation_recovery_epoch = self.isolation_recovery_epoch.clone();

        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(CONNECTION_TIMEOUT_MS).await;
            let is_current_attempt = {
                let attempts = attempt_ids.read().unwrap_or_else(|e| e.into_inner());
                matches!(attempts.get(&node), Some(id) if *id == attempt_id)
            };
            let still_connecting = {
                let states = conn_states.read().unwrap_or_else(|e| e.into_inner());
                matches!(states.get(&node), Some(ConnectionState::Connecting))
            };
            let ice_state = peer.pc.ice_connection_state();
            let ice_alive = matches!(
                ice_state,
                web_sys::RtcIceConnectionState::Connected
                    | web_sys::RtcIceConnectionState::Completed
            );
            let channel_count = {
                let channels = peer.channels.read().unwrap_or_else(|e| e.into_inner());
                channels.len()
            };
            let dc_alive = {
                let channels = peer.channels.read().unwrap_or_else(|e| e.into_inner());
                channels.values().any(|dc| {
                    matches!(
                        dc.ready_state(),
                        web_sys::RtcDataChannelState::Open
                            | web_sys::RtcDataChannelState::Connecting
                    )
                })
            };

            if is_current_attempt && still_connecting && (!ice_alive || !dc_alive) {
                {
                    let mut peers_lock = peers.write().unwrap_or_else(|e| e.into_inner());
                    peers_lock.remove(&node);
                }
                {
                    let mut senders_lock = senders.write().unwrap_or_else(|e| e.into_inner());
                    senders_lock.remove(&node);
                }
                {
                    let mut states = conn_states.write().unwrap_or_else(|e| e.into_inner());
                    states.insert(node.clone(), ConnectionState::Disconnected);
                }
                {
                    let mut attempts = attempt_ids.write().unwrap_or_else(|e| e.into_inner());
                    attempts.remove(&node);
                }
                {
                    let mut pending = pending_candidates
                        .write()
                        .unwrap_or_else(|e| e.into_inner());
                    pending.remove(&node);
                }
                // Captured before `close_all()` below, which itself changes
                // `connection_state()` -- Fix 4 wants the state that produced
                // the timeout, not the state after we tore it down.
                let pc_state = peer.pc.connection_state();
                peer.close_all(&node);
                schedule_isolation_recovery(
                    signaler,
                    conn_states,
                    attempt_ids,
                    isolation_recovery_epoch,
                );
                // Fix 4: report enough state here to tell, on the next
                // recurrence, whether this stalled at ICE, DTLS, or SCTP
                // without needing to reproduce it live -- `ice_state` is the
                // ICE layer, `pc_state` folds in DTLS (and ICE), and
                // `channel_count` (vs. `dc_alive` above being false) tells us
                // whether the DataChannels were ever created on this peer at
                // all (offerer side, always > 0 by construction) or never
                // arrived via `ondatachannel` (answerer side stalled at SCTP
                // -- the "ICE Connected but still times out" case this whole
                // restart mechanism's livelock produced).
                tracing::warn!(
                    "Connection timeout to {}. fail-fast cleanup (attempt_id={}, ice_state={:?}, pc_state={:?}, channels={}).",
                    node.0,
                    attempt_id,
                    ice_state,
                    pc_state,
                    channel_count
                );
            }
        });
    }

    pub fn close_all_peer_connections(&self) {
        self.stop_session_sweeper();
        self.isolation_recovery_epoch
            .fetch_add(1, Ordering::Relaxed);

        let peers = {
            let mut lock = self.peers.write().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *lock)
        };

        for (node, peer) in peers {
            peer.close_all(&node);
        }

        self.peer_senders
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.connection_states
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.connection_attempt_ids
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.pending_candidates
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.restarted_peers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.peer_epochs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    async fn renegotiate_peer(
        &self,
        remote_id: &NodeId,
        peer: &Arc<Peer>,
    ) -> mistlib_core::error::Result<()> {
        let state_ok = {
            let lock = self
                .connection_states
                .read()
                .unwrap_or_else(|e| e.into_inner());
            matches!(
                lock.get(remote_id),
                Some(ConnectionState::Connecting) | Some(ConnectionState::Connected)
            )
        };
        if !state_ok {
            return Err(mistlib_core::error::MistError::Internal(format!(
                "Offer precondition failed: invalid connection state for {}",
                remote_id.0
            )));
        }

        // Serializes this whole create-offer-then-apply sequence against any
        // other renegotiation of this same peer -- another concurrent
        // `renegotiate_peer` call (e.g. two `publish_local_track` calls fired
        // back to back for a screen-share's video and audio tracks, neither
        // awaited before the next starts), an inbound `apply_offer_in_place`,
        // or an ICE restart. See `Peer::negotiating`'s doc comment for the
        // exact race this closes: without it, two racing offers can both
        // observe `Stable` and both call `createOffer`, but only one's
        // `setLocalDescription` lands first -- the other's now-stale offer
        // gets rejected by Chrome with `InvalidModificationError: SDP is
        // modified in a non-acceptable way`.
        let _negotiating = peer.negotiating.lock().await;

        if peer.pc.signaling_state() != RtcSignalingState::Stable {
            tracing::debug!(
                "Skipping renegotiation with {} because signaling state is not stable",
                remote_id.0
            );
            return Err(mistlib_core::error::MistError::Internal(format!(
                "Offer precondition failed: signaling state is not stable for {}",
                remote_id.0
            )));
        }

        let ice_state = peer.pc.ice_connection_state();
        if matches!(
            ice_state,
            RtcIceConnectionState::Failed
                | RtcIceConnectionState::Closed
                | RtcIceConnectionState::Disconnected
        ) {
            return Err(mistlib_core::error::MistError::Internal(format!(
                "Offer precondition failed: unstable ice state {:?} for {}",
                ice_state, remote_id.0
            )));
        }

        let offer = match JsFuture::from(peer.pc.create_offer()).await {
            Ok(offer) => offer,
            Err(e) => {
                let room_id = self
                    .room_id
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                rollback_to_stable_on_failure(peer, remote_id, &room_id).await;
                return Err(mistlib_core::error::MistError::Internal(format!("{:?}", e)));
            }
        };
        let sdp = Reflect::get(&offer, &JsValue::from_str("sdp"))
            .map_err(|_| {
                mistlib_core::error::MistError::Internal("No SDP field in offer".to_string())
            })?
            .as_string()
            .ok_or_else(|| {
                mistlib_core::error::MistError::Internal("SDP is not a string".to_string())
            })?;

        let sdp_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        sdp_init.set_sdp(&sdp);
        if let Err(e) = JsFuture::from(peer.pc.set_local_description(&sdp_init)).await {
            // A rejection here (Chrome's `InvalidModificationError: SDP is
            // modified in a non-acceptable way` is exactly what a lost race
            // against a concurrent renegotiation on this peer used to
            // produce before `Peer::negotiating` existed) can leave the
            // connection sitting in a non-`Stable` signaling state with no
            // local description that actually matches it. Roll back rather
            // than returning with the peer wedged: every future
            // `renegotiate_peer`/`apply_offer_in_place` call for this peer
            // would otherwise keep failing "signaling state is not stable"
            // forever, since nothing else ever clears that state back to
            // `Stable`.
            let room_id = self
                .room_id
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            rollback_to_stable_on_failure(peer, remote_id, &room_id).await;
            return Err(mistlib_core::error::MistError::Internal(format!("{:?}", e)));
        }

        let room_id = self
            .room_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.signaler
            .send_signaling(
                remote_id,
                MessageContent::Data(SignalingData {
                    sender_id: self.local_node_id.clone(),
                    receiver_id: remote_id.clone(),
                    room_id,
                    data: sdp,
                    signaling_type: SignalingType::Offer,
                }),
            )
            .await
    }

    async fn handle_offer(
        &self,
        remote_id: NodeId,
        payload: String,
    ) -> mistlib_core::error::Result<()> {
        let existing_peer = {
            let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
            peers.get(&remote_id).cloned()
        };
        let peer_exists = existing_peer.is_some();
        // See `restarted_peers`' field doc: consumed here, not just read, so
        // this Offer is the one-shot "next Request or Offer" that clears the
        // latch -- regardless of which `OfferAction` it ultimately maps to.
        // The epoch itself is kept (not just the bool) so it can be stamped
        // into `peer_epochs` below once the resulting peer is actually built
        // -- see `peer_epochs`' field doc.
        let remote_restarted_epoch = self.take_remote_restarted(&remote_id);
        let signaling_snapshot = existing_peer
            .as_ref()
            .map(|peer| match peer.pc.signaling_state() {
                RtcSignalingState::Stable => SignalingSnapshot::Stable,
                RtcSignalingState::HaveLocalOffer => SignalingSnapshot::HaveLocalOffer,
                _ => SignalingSnapshot::Other,
            })
            .unwrap_or(SignalingSnapshot::Other);

        let action = {
            let mut states = self
                .connection_states
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let state = states.get(&remote_id).copied();
            let active_connections = active_connection_count(states.values().copied());
            let max_connections = self.max_connections.load(Ordering::Relaxed) as usize;

            let action = offer_action_for_snapshot(
                peer_exists,
                remote_restarted_epoch.is_some(),
                state,
                active_connections,
                max_connections,
                signaling_snapshot,
            );
            if let OfferAction::Accept {
                newly_reserved: true,
            } = action
            {
                states.insert(remote_id.clone(), ConnectionState::Connecting);
            }
            action
        };

        let newly_reserved = match action {
            OfferAction::YieldAndApply => {
                // Perfect negotiation: we have our own offer in flight on
                // this peer (HaveLocalOffer) and an inbound offer just
                // crossed it. wasm is unconditionally polite -- yield rather
                // than ignore (see `offer_guard::OfferAction::YieldAndApply`
                // for the full reasoning, including why this can't be
                // conditioned on peer id the way textbook perfect
                // negotiation is: the native peer we're most likely racing
                // against can never roll back its own offer). Routed through
                // `apply_offer_in_place`, which now accepts `HaveLocalOffer`
                // as well as `Stable`: `set_remote_description` with an
                // offer while `HaveLocalOffer` is Chrome's spec-mandated
                // implicit rollback, so the same
                // set_remote_description -> create_answer ->
                // set_local_description sequence applies unmodified.
                tracing::info!(
                    "[Perfect negotiation] yielding our in-flight offer to {}'s crossed offer",
                    remote_id.0
                );
                let peer = existing_peer
                    .expect("YieldAndApply is only returned when an existing peer was found");
                return self.apply_offer_in_place(remote_id, payload, peer).await;
            }
            OfferAction::ReplacePeer => {
                // The existing `RTCPeerConnection` belongs to a session of
                // `remote_id` that no longer exists (see `OfferAction::ReplacePeer`'s
                // doc) -- tear it down completely rather than renegotiating
                // it, then fall through to the same create-from-scratch path
                // `Accept` uses below.
                tracing::info!(
                    "Replacing stale peer connection for {} after detected restart",
                    remote_id.0
                );
                // Same teardown `RequestAction::CleanupAndConnect` uses
                // (`cleanup_peer_connection`, `close_pc = true`): closes the
                // old `Peer` and removes it from `peers`/`peer_senders`/
                // `connection_attempt_ids`/`pending_candidates`, and resets
                // `connection_states` to `Disconnected`. `IsolationRecovery::Skip`
                // for the same reason as that call site: the create-from-scratch
                // path immediately below rebuilds the connection, so this is
                // not a genuine isolation -- see `IsolationRecovery`'s doc.
                self.cleanup_peer_connection(&remote_id, true, IsolationRecovery::Skip);
                // `cleanup_peer_connection` doesn't touch `disconnected_since`
                // (see its own call sites) -- clear it explicitly here so a
                // leftover ICE-disconnected grace entry from the dead
                // instance can't make the brand-new peer's own first
                // `Disconnected` transition look like a continuation of that
                // old grace (`Entry::Occupied` in `Peer::setup_handlers`)
                // instead of a fresh one, which would suppress its own
                // ICE-restart trigger.
                self.disconnected_since
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&remote_id);
                // Reservation accounting: this peer was already counted as
                // active (it had a `connection_states` entry, however stale)
                // before this replacement started, so this is not a *new*
                // reservation -- `newly_reserved = false` here mirrors
                // `Accept { newly_reserved: false }`'s semantics: on a later
                // `create_pc` failure below, `create_failure_rollback` leaves
                // the (now `Disconnected`, from the cleanup above)
                // `connection_states` entry alone instead of removing it,
                // rather than double-reserving a slot the dead peer already
                // held or releasing one this replacement still needs.
                false
            }
            OfferAction::IgnoreAtCapacity => {
                tracing::debug!(
                    "Ignoring offer from {} because max_connections is reached",
                    remote_id.0
                );
                return Ok(());
            }
            OfferAction::ApplyInPlace => {
                let peer = existing_peer
                    .expect("ApplyInPlace is only returned when an existing peer was found");
                return self.apply_offer_in_place(remote_id, payload, peer).await;
            }
            OfferAction::DeferTransient => {
                // A transient signaling state on a *live* peer (e.g. the
                // polite side mid-glare, or our own answer to an earlier
                // offer still being applied) -- leave the connection alone
                // rather than discarding a healthy peer's DataChannels/tracks
                // over what is usually a momentary race. This offer goes
                // unanswered; the remote's retry (or the next renegotiation
                // once signaling settles back to Stable) recovers it.
                tracing::debug!(
                    "Deferring offer from {} because the existing peer's signaling state is transient",
                    remote_id.0
                );
                return Ok(());
            }
            OfferAction::Accept { newly_reserved } => newly_reserved,
        };

        let peer = match self.create_pc(remote_id.clone()) {
            Ok(peer) => peer,
            Err(err) => {
                if create_failure_rollback(newly_reserved)
                    == OfferCreateFailureRollback::RemoveReservation
                {
                    let mut states = self
                        .connection_states
                        .write()
                        .unwrap_or_else(|e| e.into_inner());
                    states.remove(&remote_id);
                }
                return Err(mistlib_core::error::MistError::Internal(format!(
                    "{:?}",
                    err
                )));
            }
        };

        let old_peer = {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            peers.insert(remote_id.clone(), peer.clone())
        };
        if let Some(old_peer) = old_peer {
            old_peer.close_all(&remote_id);
        }

        let attempt_id = self.reserve_connection_attempt(&remote_id);
        self.spawn_connection_watchdog(remote_id.clone(), attempt_id, peer.clone());

        let result = async {
            // Guards against the same race `renegotiate_peer`/
            // `apply_offer_in_place` guard against: `peer` is already visible
            // in `self.peers` at this point (inserted just above), so a
            // `publish_local_track` call landing before this brand-new
            // peer's first answer completes could otherwise race this
            // sequence's `set_remote_description`/`create_answer`/
            // `set_local_description`. Uncontended in the overwhelmingly
            // common case (a fresh peer nothing else knows about yet).
            let _negotiating = peer.negotiating.lock().await;

            if peer.pc.signaling_state() != RtcSignalingState::Stable {
                return Err(mistlib_core::error::MistError::Internal(format!(
                    "Offer precondition failed: signaling is not stable for {}",
                    remote_id.0
                )));
            }

            let sdp = sdp_from_signaling_payload(&payload);
            let sdp_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
            sdp_init.set_sdp(&sdp);
            JsFuture::from(peer.pc.set_remote_description(&sdp_init))
                .await
                .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;

            let answer = JsFuture::from(peer.pc.create_answer())
                .await
                .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;
            let answer_sdp = Reflect::get(&answer, &JsValue::from_str("sdp"))
                .map_err(|_| {
                    mistlib_core::error::MistError::Internal("No SDP field in answer".to_string())
                })?
                .as_string()
                .ok_or_else(|| {
                    mistlib_core::error::MistError::Internal("SDP is not a string".to_string())
                })?;

            let answer_init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
            answer_init.set_sdp(&answer_sdp);
            JsFuture::from(peer.pc.set_local_description(&answer_init))
                .await
                .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;

            self.apply_pending_candidates(&remote_id, &peer);

            let room_id = self
                .room_id
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            self.signaler
                .send_signaling(
                    &remote_id,
                    MessageContent::Data(SignalingData {
                        sender_id: self.local_node_id.clone(),
                        receiver_id: remote_id.clone(),
                        room_id,
                        data: answer_sdp,
                        signaling_type: SignalingType::Answer,
                    }),
                )
                .await?;

            Ok(())
        }
        .await;

        if result.is_err() {
            // Genuine failure, nothing queued up to replace this peer --
            // ordinary teardown, unlike the `ReplacePeer` cleanup above.
            self.cleanup_peer_connection(&remote_id, true, IsolationRecovery::Schedule);
        } else {
            // The peer this offer built is now live -- if it was built in
            // response to a restart (`OfferAction::ReplacePeer`, or `Accept`
            // for a peer that restarted before we'd ever seen it), stamp the
            // epoch it was built for so a duplicate/late `Rejoin` carrying an
            // epoch we've already acted on doesn't tear it down again. See
            // `peer_epochs`' field doc.
            if let Some(epoch) = remote_restarted_epoch {
                self.peer_epochs
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(remote_id.clone(), epoch);
            }

            if self.has_published_tracks() {
                // New-peer hook, answer-side completion -- mirrors
                // `mistlib-native`'s `signaling::handle_offer`: `create_pc`
                // (called above) already attached every published track to
                // `peer`'s RTCPeerConnection before we answered, but per JSEP
                // the answer we just sent cannot introduce m= sections beyond
                // what the remote's offer contained -- and a brand-new peer's
                // first offer is data-channels-only. So the published tracks'
                // senders exist on the connection but are not yet negotiated,
                // and without this follow-up offer of our own the new peer
                // would never receive already-running tracks (e.g. a
                // screen-share started before they joined) whenever *they*
                // initiated the handshake. Best-effort by design, same as
                // native: a failure here must not undo the connection the
                // answer just established, so it defers to the
                // `needs_track_reconcile` recovery path instead of erroring.
                if let Err(err) = self.renegotiate_peer(&remote_id, &peer).await {
                    peer.needs_track_reconcile
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    tracing::warn!(
                        "Failed to renegotiate published tracks with new peer {} ({}); deferred to recovery",
                        remote_id.0,
                        err
                    );
                }
            }
        }

        result
    }

    /// Applies an inbound offer directly to an existing `RTCPeerConnection`
    /// instead of discarding it and starting over: `set_remote_description`
    /// -> `create_answer` -> `set_local_description` -> send the answer back
    /// over signaling. This is what makes renegotiation (a peer publishing a
    /// new track, or the remote doing an ICE restart) keep the existing
    /// DataChannels/tracks alive, matching `mistlib-native`'s `apply_offer`.
    /// Mirrors the answer half of `handle_offer`'s brand-new-connection path,
    /// but -- also matching native -- does not tear the peer down via
    /// `cleanup_peer_connection` on error; a failed renegotiation leaves the
    /// still-live connection (its DataChannels/tracks) as-is for the
    /// caller/next attempt to sort out. It does, however, take
    /// `Peer::negotiating` for the whole sequence (same as
    /// `renegotiate_peer`, to close the race between the two) and roll the
    /// *signaling* state back to `Stable` on failure, so a rejected answer
    /// doesn't leave the peer permanently stuck failing every later
    /// negotiation attempt's "signaling state is not stable" precondition.
    ///
    /// Entered from two `OfferAction`s: `ApplyInPlace` (peer is `Stable`, an
    /// ordinary renegotiation) and `YieldAndApply` (peer is `HaveLocalOffer`,
    /// perfect-negotiation glare against our own in-flight offer -- see that
    /// variant's doc). Both run the exact same `set_remote_description` call
    /// below; the difference is entirely in what Chrome does with it
    /// internally -- applied directly from `Stable`, or, from
    /// `HaveLocalOffer`, as a spec-mandated *implicit rollback* that discards
    /// our own local offer first. For the latter, `needs_track_reconcile` is
    /// set before proceeding so whatever that abandoned offer was carrying
    /// (a published track, an ICE restart) gets re-proposed by this
    /// function's existing success-path drain once we settle back to
    /// `Stable`, instead of silently disappearing.
    async fn apply_offer_in_place(
        &self,
        remote_id: NodeId,
        payload: String,
        peer: Arc<Peer>,
    ) -> mistlib_core::error::Result<()> {
        let _negotiating = peer.negotiating.lock().await;

        // `handle_offer`'s `Stable`/`HaveLocalOffer` snapshot that routed
        // this call here was taken before this lock was acquired; re-check
        // now that we actually hold it, in case a concurrent renegotiation on
        // this same peer finished (or moved to some other state entirely) in
        // the meantime. `HaveLocalOffer` is accepted alongside `Stable`
        // specifically for the perfect-negotiation yield case
        // (`OfferAction::YieldAndApply`): the `set_remote_description` call
        // below performs Chrome's implicit rollback of our own in-flight
        // offer when called from that state.
        let signaling_state = peer.pc.signaling_state();
        if !matches!(
            signaling_state,
            RtcSignalingState::Stable | RtcSignalingState::HaveLocalOffer
        ) {
            return Err(mistlib_core::error::MistError::Internal(format!(
                "Offer precondition failed: signaling is not stable for {}",
                remote_id.0
            )));
        }
        if signaling_state == RtcSignalingState::HaveLocalOffer {
            // We're yielding: our own offer is about to be implicitly rolled
            // back and discarded by the `set_remote_description` call just
            // below. Whatever it was carrying must not simply vanish -- mark
            // it for re-proposal once signaling settles back to `Stable`.
            // The drain already at the end of this function's success path
            // (shared with the ordinary `ApplyInPlace` case) fires
            // unconditionally, so setting the flag here is sufficient; no
            // separate handling is needed downstream for the yield case.
            tracing::info!(
                "[Perfect negotiation] yielding: our offer to {} is being implicitly rolled back by their crossed offer",
                remote_id.0
            );
            peer.needs_track_reconcile
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        let sdp = sdp_from_signaling_payload(&payload);
        let sdp_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        sdp_init.set_sdp(&sdp);
        if let Err(e) = JsFuture::from(peer.pc.set_remote_description(&sdp_init)).await {
            let room_id = self
                .room_id
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            rollback_to_stable_on_failure(&peer, &remote_id, &room_id).await;
            return Err(mistlib_core::error::MistError::Internal(format!("{:?}", e)));
        }

        let answer = match JsFuture::from(peer.pc.create_answer()).await {
            Ok(answer) => answer,
            Err(e) => {
                let room_id = self
                    .room_id
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                rollback_to_stable_on_failure(&peer, &remote_id, &room_id).await;
                return Err(mistlib_core::error::MistError::Internal(format!("{:?}", e)));
            }
        };
        let answer_sdp = Reflect::get(&answer, &JsValue::from_str("sdp"))
            .map_err(|_| {
                mistlib_core::error::MistError::Internal("No SDP field in answer".to_string())
            })?
            .as_string()
            .ok_or_else(|| {
                mistlib_core::error::MistError::Internal("SDP is not a string".to_string())
            })?;

        let answer_init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        answer_init.set_sdp(&answer_sdp);
        if let Err(e) = JsFuture::from(peer.pc.set_local_description(&answer_init)).await {
            let room_id = self
                .room_id
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            rollback_to_stable_on_failure(&peer, &remote_id, &room_id).await;
            return Err(mistlib_core::error::MistError::Internal(format!("{:?}", e)));
        }

        self.apply_pending_candidates(&remote_id, &peer);

        let room_id = self
            .room_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.signaler
            .send_signaling(
                &remote_id,
                MessageContent::Data(SignalingData {
                    sender_id: self.local_node_id.clone(),
                    receiver_id: remote_id.clone(),
                    room_id: room_id.clone(),
                    data: answer_sdp,
                    signaling_type: SignalingType::Answer,
                }),
            )
            .await?;

        // Signaling just settled back to Stable -- if a publish/unpublish
        // deferred its renegotiation on this peer (`needs_track_reconcile`),
        // this is a natural moment to run it: the primary trigger (the
        // ICE-Connected arm in `Peer::setup_handlers`) can itself get
        // deferred again if it fires while an inbound renegotiation is still
        // settling, and this inbound renegotiation completing may be the
        // last state-change event the peer produces. Spawned rather than
        // awaited because `reconcile_peer_tracks` -> `renegotiate_peer`
        // takes `Peer::negotiating`, which this function still holds.
        if peer
            .needs_track_reconcile
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let remote_id = remote_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let Some(transport) = crate::app::session_webrtc(&room_id) else {
                    return;
                };
                transport.reconcile_peer_tracks(&remote_id).await;
            });
        }

        Ok(())
    }
}

// --- Liveness / reconnect grace ---
impl WasmWebRtcTransport {
    /// Entry point for `OverlayAction::SuspectDisconnected`: only transitions a
    /// currently-`Connected` peer into the grace flow, tagged as
    /// liveness-suspect-originated. A peer that isn't `Connected` (already
    /// reconnecting, disconnected, or unknown) is left untouched -- there's
    /// either already a grace period running (whatever its origin) or nothing
    /// to suspect in the first place.
    pub(crate) fn mark_suspect_disconnected(&self, node: &NodeId) -> bool {
        {
            let mut states = self
                .connection_states
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if states.get(node) != Some(&ConnectionState::Connected) {
                return false;
            }
            states.insert(node.clone(), ConnectionState::Reconnecting);
        }

        let started_now = {
            let mut disconnected = self
                .disconnected_since
                .write()
                .unwrap_or_else(|e| e.into_inner());
            match disconnected.entry(node.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(DisconnectGrace {
                        started_at: Instant::now(),
                        origin: GraceOrigin::LivenessSuspect,
                    });
                    true
                }
                std::collections::hash_map::Entry::Occupied(_) => false,
            }
        };
        tracing::warn!(
            "[CS] Suspect disconnected (grace started={}): {}",
            started_now,
            node.0
        );
        true
    }

    /// Entry point for `OverlayAction::ClearSuspect`: cancels the current grace
    /// period only if it was started by `mark_suspect_disconnected`. A grace
    /// period started by ICE `Disconnected` is left alone -- only ICE's own
    /// recovery signal is allowed to end that one.
    pub(crate) fn cancel_suspect_grace(&self, node: &NodeId) -> bool {
        let is_suspect_origin = {
            let disconnected = self
                .disconnected_since
                .read()
                .unwrap_or_else(|e| e.into_inner());
            matches!(
                disconnected.get(node),
                Some(DisconnectGrace {
                    origin: GraceOrigin::LivenessSuspect,
                    ..
                })
            )
        };
        if !is_suspect_origin {
            return false;
        }

        {
            let mut states = self
                .connection_states
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if !states.contains_key(node) {
                return false;
            }
            states.insert(node.clone(), ConnectionState::Connected);
        }
        self.disconnected_since
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(node);
        tracing::info!("[CS] cleared liveness-suspect grace, recovered: {}", node.0);
        true
    }

    pub fn get_active_connection_states(&self) -> Vec<(NodeId, ConnectionState)> {
        let states = self
            .connection_states
            .read()
            .unwrap_or_else(|e| e.into_inner());
        states
            .iter()
            .filter(|(_, &s)| {
                matches!(
                    s,
                    ConnectionState::Connected
                        | ConnectionState::Connecting
                        | ConnectionState::Reconnecting
                )
            })
            .map(|(id, &s)| (id.clone(), s))
            .collect()
    }
}

/// Schedules a delayed check of whether we're isolated (no live/in-flight
/// connections to anyone) and, if so, rotates our own signaling identity via
/// `signaler.reset_session()` to try to recover. `connection_attempt_ids` is
/// threaded through here (not just `connection_states`) for the same reason
/// `is_isolated` requires it -- see that function's doc. Both maps are read
/// only after the delay elapses (never snapshotted before it), so the check
/// reflects what's actually true at fire time, including any reconnect that
/// started during the delay.
fn schedule_isolation_recovery(
    signaler: Arc<dyn Signaler>,
    connection_states: Arc<RwLock<HashMap<NodeId, ConnectionState>>>,
    connection_attempt_ids: Arc<RwLock<HashMap<NodeId, u32>>>,
    recovery_epoch: Arc<AtomicU32>,
) {
    let expected_epoch = recovery_epoch
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    wasm_bindgen_futures::spawn_local(async move {
        gloo_timers::future::TimeoutFuture::new(ISOLATION_RECOVERY_DELAY_MS).await;
        if recovery_epoch.load(Ordering::SeqCst) != expected_epoch {
            return;
        }
        let states: Vec<ConnectionState> = connection_states
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .copied()
            .collect();
        let in_flight_attempts = connection_attempt_ids
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        if is_isolated(states, in_flight_attempts) {
            if let Err(err) = signaler.reset_session().await {
                tracing::warn!("isolated signaling session reset failed: {:?}", err);
            }
        }
    });
}

/// Queues a waiter for `method`'s channel to drain back down to
/// `BUFFERED_AMOUNT_LOW_THRESHOLD`, bounded by `BUFFERED_AMOUNT_WAIT_TIMEOUT_MS`.
/// The actual `onbufferedamountlow` handler is registered once and
/// permanently in `Peer::setup_dc_handlers`, which drains `waiters` on every
/// fire -- this function only pushes its own one-shot sender onto that queue
/// and awaits it, rather than swapping in its own `set_onbufferedamountlow`
/// closure. That distinction is the fix for a real bug: two concurrent
/// Reliable sends waiting on the same channel would otherwise have the
/// second wait's `set_onbufferedamountlow` silently replace the first's,
/// leaving the first to sit out the full timeout even though the channel
/// drained in time.
///
/// Returns `true` if the drain fired in time, `false` on timeout or if the
/// peer closed while waiting (`Peer::close_all` clears `waiters`, which
/// surfaces here as the receiver's sender having been dropped) -- either way
/// the caller (`send`) drops the message rather than sending into a channel
/// that's still congested (or gone). Only touches `waiters` through brief,
/// synchronous lock scopes, so no `RwLock` borrow is held across the
/// `.await` below.
async fn wait_for_buffered_amount_low(
    waiters: &Arc<RwLock<HashMap<DeliveryMethod, Vec<tokio::sync::oneshot::Sender<()>>>>>,
    method: DeliveryMethod,
) -> bool {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    {
        let mut lock = waiters.write().unwrap_or_else(|e| e.into_inner());
        let queue = lock.entry(method).or_default();
        // Opportunistically prune waiters from earlier calls that already
        // timed out (and dropped their receiver) instead of letting them
        // accumulate until the next `onbufferedamountlow` fire drains them.
        queue.retain(|tx| !tx.is_closed());
        queue.push(tx);
    }

    let timeout = gloo_timers::future::TimeoutFuture::new(BUFFERED_AMOUNT_WAIT_TIMEOUT_MS);
    matches!(
        futures::future::select(rx, timeout).await,
        futures::future::Either::Left((Ok(()), _))
    )
}

/// Fires an ICE-restart offer on `peer`'s existing connection: `createOffer`
/// with `iceRestart: true` -> `setLocalDescription` -> send it through the
/// normal offer signaling envelope. Called from `Peer::setup_handlers`'s
/// `oniceconnectionstatechange` handler when `ice_restart::should_trigger_ice_restart`
/// says so (new disconnected-grace period, we're the initiator, signaling
/// still stable). Deliberately does not go through `renegotiate_peer` --
/// that function rejects a `Disconnected` ice_state on purpose, but this
/// path exists precisely because ice_state just became `Disconnected`.
///
/// Takes `Peer::negotiating` for the same reason `renegotiate_peer` and
/// `apply_offer_in_place` do -- an ICE restart's createOffer/setLocalDescription
/// pair mutates signaling state just like theirs, so it must not race a
/// renegotiation triggered concurrently by, say, a `publish_local_track` call
/// landing at the same moment as the ICE disconnect.
///
/// The remote side receives this as an ordinary `Offer`; since its signaling
/// state is still `Stable`, `handle_offer` takes the `ApplyInPlace` branch
/// and answers it in-place, same as any other renegotiation. A failure here
/// (createOffer/setLocalDescription rejected, or the signaling send fails)
/// is left to the existing disconnected-grace expiry/cleanup as a safety
/// net -- no separate rollback is attempted (unlike `renegotiate_peer`'s),
/// since a stuck-non-Stable state here is subsumed by that same expiry path
/// eventually tearing down and recreating the peer regardless.
async fn trigger_ice_restart(
    peer: Arc<Peer>,
    signaler: Arc<dyn Signaler>,
    local_id: NodeId,
    remote_id: NodeId,
    room_id: String,
) {
    let _negotiating = peer.negotiating.lock().await;
    let pc = &peer.pc;

    if pc.signaling_state() != RtcSignalingState::Stable {
        tracing::debug!(
            "Skipping ICE restart for {}: signaling state no longer stable",
            remote_id.0
        );
        return;
    }

    let options = RtcOfferOptions::new();
    options.set_ice_restart(true);
    let offer = match JsFuture::from(pc.create_offer_with_rtc_offer_options(&options)).await {
        Ok(offer) => offer,
        Err(err) => {
            tracing::warn!(
                "ICE restart createOffer failed for {}: {:?}",
                remote_id.0,
                err
            );
            return;
        }
    };
    let sdp = match Reflect::get(&offer, &JsValue::from_str("sdp"))
        .ok()
        .and_then(|v| v.as_string())
    {
        Some(sdp) => sdp,
        None => {
            tracing::warn!("ICE restart offer for {} had no SDP field", remote_id.0);
            return;
        }
    };

    let sdp_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    sdp_init.set_sdp(&sdp);
    if let Err(err) = JsFuture::from(pc.set_local_description(&sdp_init)).await {
        tracing::warn!(
            "ICE restart setLocalDescription failed for {}: {:?}",
            remote_id.0,
            err
        );
        return;
    }

    tracing::info!("Sending ICE restart offer to {}", remote_id.0);
    if let Err(err) = signaler
        .send_signaling(
            &remote_id,
            MessageContent::Data(SignalingData {
                sender_id: local_id,
                receiver_id: remote_id.clone(),
                room_id,
                data: sdp,
                signaling_type: SignalingType::Offer,
            }),
        )
        .await
    {
        tracing::warn!(
            "ICE restart offer signaling failed for {}: {:?}",
            remote_id.0,
            err
        );
    }
}

/// Best-effort recovery for a failed renegotiation on `pc`: if the failure
/// left signaling state anywhere other than `Stable`, issues a `rollback` on
/// whichever side actually has a pending description, clearing it back to
/// `Stable`. Called from `renegotiate_peer` and `apply_offer_in_place` after
/// a `create_offer`/`create_answer`/`set_local_description`/
/// `set_remote_description` step fails while a caller (protected by
/// `Peer::negotiating`) is mid-negotiation.
///
/// Without this, a rejected `setLocalDescription`/`setRemoteDescription` --
/// e.g. Chrome's `InvalidModificationError: SDP is modified in a
/// non-acceptable way` -- can leave the connection sitting in
/// `HaveLocalOffer`/`HaveRemoteOffer` with no description actually applied to
/// match: every later negotiation attempt for this peer (a retried publish,
/// an unpublish, an ICE restart) would then keep failing the "signaling
/// state is not stable" precondition forever, since nothing else ever moves
/// it back to `Stable`.
///
/// Picks `set_local_description`/`set_remote_description` based on which
/// side is actually pending (`HaveLocalOffer`/`HaveLocalPranswer` vs.
/// `HaveRemoteOffer`/`HaveRemotePranswer`) since WebRTC's rollback is
/// direction-specific: rolling back *our* pending offer/pranswer is a local
/// rollback, rolling back the remote's is a remote one. Best-effort: the
/// rollback call itself can fail too (e.g. the connection is already
/// closing) -- that's logged and swallowed, since the original error is what
/// the caller should see and act on.
async fn rollback_to_stable_on_failure(peer: &Arc<Peer>, remote_id: &NodeId, room_id: &str) {
    let pc = &peer.pc;
    let signaling_state = pc.signaling_state();
    let rollback = RtcSessionDescriptionInit::new(RtcSdpType::Rollback);
    match signaling_state {
        RtcSignalingState::HaveLocalOffer | RtcSignalingState::HaveLocalPranswer => {
            if let Err(err) = JsFuture::from(pc.set_local_description(&rollback)).await {
                tracing::warn!(
                    "Rollback to stable (local) failed for {} after a negotiation error: {:?}",
                    remote_id.0,
                    err
                );
            }
        }
        RtcSignalingState::HaveRemoteOffer | RtcSignalingState::HaveRemotePranswer => {
            if let Err(err) = JsFuture::from(pc.set_remote_description(&rollback)).await {
                tracing::warn!(
                    "Rollback to stable (remote) failed for {} after a negotiation error: {:?}",
                    remote_id.0,
                    err
                );
            }
        }
        RtcSignalingState::Stable | RtcSignalingState::Closed => {}
        other => {
            tracing::warn!(
                "No rollback path for signaling state {:?} on {} after a negotiation error",
                other,
                remote_id.0
            );
        }
    }

    // A negotiation failure that lands back on `Stable` is, from
    // `Peer::needs_track_reconcile`'s perspective, exactly the kind of
    // settle-point the ICE-Connected/Answer/`apply_offer_in_place` drains
    // exist for -- but none of those three fire here: this isn't an ICE
    // transition, and it isn't an *applied* answer/offer (the whole point of
    // this function is that the description was rejected). Without this, a
    // publish/unpublish that deferred while some *other*, unrelated
    // negotiation was in flight on this peer -- and that other negotiation
    // then failed instead of succeeding -- would leave the flag `true` on a
    // peer that just settled back to `Stable` with nothing left to ever
    // check it again (ICE stays healthy throughout a pure signaling
    // rejection, so the edge-triggered ICE-Connected arm doesn't refire
    // either). Treat this rollback as a fourth drain point, same
    // spawn-not-await pattern as the other three: every caller here still
    // holds `Peer::negotiating` at this point, and `reconcile_peer_tracks` ->
    // `renegotiate_peer` needs to take that same lock.
    if peer
        .needs_track_reconcile
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        let remote_id = remote_id.clone();
        let room_id = room_id.to_string();
        wasm_bindgen_futures::spawn_local(async move {
            let Some(transport) = crate::app::session_webrtc(&room_id) else {
                return;
            };
            transport.reconcile_peer_tracks(&remote_id).await;
        });
    }
}

fn sdp_from_signaling_payload(payload: &str) -> String {
    js_sys::JSON::parse(payload)
        .ok()
        .and_then(|value| Reflect::get(&value, &JsValue::from_str("sdp")).ok())
        .and_then(|value| value.as_string())
        .unwrap_or_else(|| payload.to_string())
}

fn parse_and_add_candidate(node: &NodeId, peer: &Peer, cand_json: &str) -> Result<(), JsValue> {
    let cand_obj = js_sys::JSON::parse(cand_json)?;
    let candidate_str = Reflect::get(&cand_obj, &JsValue::from_str("candidate"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    let sdp_mid = Reflect::get(&cand_obj, &JsValue::from_str("sdpMid"))
        .ok()
        .and_then(|v| v.as_string());
    let sdp_m_line_index = Reflect::get(&cand_obj, &JsValue::from_str("sdpMLineIndex"))
        .ok()
        .and_then(|v| v.as_f64())
        .map(|v| v as u16);

    let cand_init = RtcIceCandidateInit::new(&candidate_str);
    if let Some(mid) = sdp_mid {
        cand_init.set_sdp_mid(Some(&mid));
    }
    if let Some(m_line_index) = sdp_m_line_index {
        cand_init.set_sdp_m_line_index(Some(m_line_index));
    }

    // `addIceCandidate` returns a Promise, not a synchronous Result: a
    // rejection (malformed candidate, closed connection, etc.) has to be
    // awaited to be observed at all. Spawn rather than `.await` here since
    // this function is called from synchronous call sites; the callers'
    // own `Err` handling above only covers the synchronous parse/construct
    // steps, so this is the only place a rejection surfaces.
    let promise = peer
        .pc
        .add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&cand_init));
    let node = node.clone();
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(err) = JsFuture::from(promise).await {
            tracing::warn!("addIceCandidate rejected for {}: {:?}", node.0, err);
        }
    });

    Ok(())
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Transport for WasmWebRtcTransport {
    async fn start(
        &self,
        handler: Arc<dyn NetworkEventHandler>,
    ) -> mistlib_core::error::Result<()> {
        self.ensure_session_sweeper();

        {
            let mut lock = self.event_handler.lock().unwrap_or_else(|e| e.into_inner());
            *lock = Some(handler);
        }

        let room_id = self
            .room_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        tracing::info!(
            "Starting WasmWebRtcTransport. Sending Request to server in room: {}",
            room_id
        );
        self.request_peers().await?;

        Ok(())
    }

    async fn send(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> mistlib_core::error::Result<()> {
        let limit = self.max_message_bytes.load(Ordering::Relaxed);
        match check_message_size(data.len(), limit) {
            Err(err) => return Err(err),
            Ok(SizeCheck::NearLimit) => {
                tracing::warn!(
                    "Message to {} is {}B, at or above 80% of max_message_bytes ({}B)",
                    node.0,
                    data.len(),
                    limit
                );
            }
            Ok(SizeCheck::Ok) => {}
        }

        let node_state = self.get_connection_state(node);
        if node_state != ConnectionState::Connected {
            // The peer may still exist and be usable in a moment (mid
            // ICE-restart grace, or a fresh connection whose DataChannel
            // hasn't opened yet) rather than actually gone -- defer
            // `ReliableOrdered` sends onto the peer's own bounded queue
            // instead of failing them outright, so a burst of sends racing
            // an ICE restart doesn't just get dropped on the floor (see the
            // field report this addresses: 7+3 failed "Not connected" sends
            // around one ICE-restart recovery). Unreliable methods keep the
            // existing fail-fast behavior -- see
            // `send_queue::should_queue_reliable_send`'s doc comment for why.
            let peer = {
                let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
                peers.get(node).cloned()
            };

            if let Some(peer) = &peer {
                if should_queue_reliable_send(method, true, node_state) {
                    let dropped_oldest = {
                        let mut queue = peer.send_queue.lock().unwrap_or_else(|e| e.into_inner());
                        queue.push(data)
                    };
                    if dropped_oldest {
                        tracing::warn!(
                            "Reliable send queue for {} exceeded {} messages/{}B; dropped oldest queued message",
                            node.0,
                            MAX_QUEUED_MESSAGES,
                            MAX_QUEUED_BYTES
                        );
                    }
                    tracing::debug!(
                        "Deferring reliable send to {} (state={:?}, peer mid-recovery); queued for flush on DC reopen/ICE recovery",
                        node.0,
                        node_state
                    );
                    return Ok(());
                }
            }

            return Err(mistlib_core::error::MistError::Internal(
                "Not connected".to_string(),
            ));
        }

        let dc_opt = {
            let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
            peers.get(node).cloned().and_then(|peer| {
                let channels = peer.channels.read().unwrap_or_else(|e| e.into_inner());
                channels
                    .get(&method)
                    .cloned()
                    .or_else(|| {
                        if method == DeliveryMethod::UnreliableOrdered {
                            channels.get(&DeliveryMethod::Unreliable).cloned()
                        } else {
                            None
                        }
                    })
                    .map(|dc| (dc, peer.buffered_amount_waiters.clone()))
            })
        };

        if let Some((dc, buffered_amount_waiters)) = dc_opt {
            let ready_state = dc.ready_state();
            if ready_state == web_sys::RtcDataChannelState::Open {
                match backpressure_action(
                    dc.buffered_amount(),
                    BUFFERED_AMOUNT_HIGH_WATERMARK,
                    method,
                ) {
                    BackpressureAction::SendNow => {}
                    BackpressureAction::WaitThenSend => {
                        tracing::warn!(
                            "DataChannel to {} congested (bufferedAmount over {}B); waiting for drain",
                            node.0,
                            BUFFERED_AMOUNT_HIGH_WATERMARK
                        );
                        // WaitThenSend is only reachable for ReliableOrdered
                        // (see `backpressure_action`), and ReliableOrdered has
                        // no channel-fallback substitution above -- so
                        // `method` here always matches the channel actually
                        // in hand, which is exactly how its permanent
                        // `onbufferedamountlow` handler was keyed in
                        // `Peer::setup_dc_handlers`.
                        if !wait_for_buffered_amount_low(&buffered_amount_waiters, method).await {
                            tracing::warn!(
                                "DataChannel to {} still congested after {}ms; dropping reliable message",
                                node.0,
                                BUFFERED_AMOUNT_WAIT_TIMEOUT_MS
                            );
                            return Err(mistlib_core::error::MistError::Internal(
                                "Backpressure: bufferedAmount drain timed out".to_string(),
                            ));
                        }
                    }
                    BackpressureAction::Drop => {
                        tracing::warn!(
                            "Dropping {:?} message to {} (bufferedAmount over {}B)",
                            method,
                            node.0,
                            BUFFERED_AMOUNT_HIGH_WATERMARK
                        );
                        return Err(mistlib_core::error::MistError::Internal(
                            "Backpressure: dropped unreliable message under congestion".to_string(),
                        ));
                    }
                }

                dc.send_with_u8_array(&data).map_err(|e| {
                    tracing::error!("DataChannel send failed for {}: {:?}", node.0, e);
                    mistlib_core::error::MistError::Internal(format!("{:?}", e))
                })?;
                STATS.add_send(data.len() as u64);
                STATS.add_world_send_frame(&data);
                return Ok(());
            } else {
                if ready_state == web_sys::RtcDataChannelState::Closed
                    || ready_state == web_sys::RtcDataChannelState::Closing
                {
                    let mut states = self
                        .connection_states
                        .write()
                        .unwrap_or_else(|e| e.into_inner());
                    states.insert(node.clone(), ConnectionState::Connecting);
                }
                tracing::warn!(
                    "DataChannel for {} is not Open (state={:?})",
                    node.0,
                    ready_state
                );
            }
        } else {
            let has_peer = {
                let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
                peers.contains_key(node)
            };
            if has_peer {
                tracing::warn!(
                    "No DataChannel for {:?} for node {} (transient; peer kept)",
                    method,
                    node.0
                );
            } else if node_state != ConnectionState::Disconnected {
                tracing::warn!("No Peer for node {}", node.0);
            }
        }
        Err(mistlib_core::error::MistError::Internal(
            "Not connected".to_string(),
        ))
    }

    async fn broadcast(
        &self,
        data: Bytes,
        method: DeliveryMethod,
    ) -> mistlib_core::error::Result<()> {
        let nodes = self.get_connected_nodes();
        tracing::debug!("WasmWebRtcTransport: Broadcasting to {} nodes", nodes.len());
        for node in nodes {
            let _ = self.send(&node, data.clone(), method).await;
        }
        Ok(())
    }

    fn get_connection_state(&self, node: &NodeId) -> ConnectionState {
        self.connection_states
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(node)
            .cloned()
            .unwrap_or(ConnectionState::Disconnected)
    }

    async fn connect(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        tracing::info!("Connecting to peer: {}", node.0);

        {
            let states = self
                .connection_states
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(state) = states.get(node) {
                if matches!(
                    *state,
                    ConnectionState::Connecting
                        | ConnectionState::Connected
                        | ConnectionState::Reconnecting
                ) {
                    return Ok(());
                }
            }

            let max = self.max_connections.load(Ordering::Relaxed) as usize;
            let current = states
                .values()
                .filter(|&&s| {
                    matches!(
                        s,
                        ConnectionState::Connected
                            | ConnectionState::Connecting
                            | ConnectionState::Reconnecting
                    )
                })
                .count();
            if current >= max {
                return Ok(());
            }
        }

        {
            let mut states = self
                .connection_states
                .write()
                .unwrap_or_else(|e| e.into_inner());
            states.insert(node.clone(), ConnectionState::Connecting);
        }

        let attempt_id = self
            .next_connection_attempt_id
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        {
            let mut attempts = self
                .connection_attempt_ids
                .write()
                .unwrap_or_else(|e| e.into_inner());
            attempts.insert(node.clone(), attempt_id);
        }

        let peer = self
            .create_pc(node.clone())
            .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;

        let dc_init = RtcDataChannelInit::new();
        dc_init.set_ordered(true);
        let reliable = peer
            .pc
            .create_data_channel_with_data_channel_dict("reliable", &dc_init);
        {
            let mut channels = peer.channels.write().unwrap_or_else(|e| e.into_inner());
            channels.insert(DeliveryMethod::ReliableOrdered, reliable.clone());
        }
        let unreliable_ordered_init = RtcDataChannelInit::new();
        unreliable_ordered_init.set_ordered(true);
        unreliable_ordered_init.set_max_retransmits(0);
        let unreliable_ordered = peer.pc.create_data_channel_with_data_channel_dict(
            "unreliable-ordered",
            &unreliable_ordered_init,
        );
        {
            let mut channels = peer.channels.write().unwrap_or_else(|e| e.into_inner());
            channels.insert(
                DeliveryMethod::UnreliableOrdered,
                unreliable_ordered.clone(),
            );
        }

        let unreliable_init = RtcDataChannelInit::new();
        unreliable_init.set_ordered(false);
        unreliable_init.set_max_retransmits(0);
        let unreliable = peer
            .pc
            .create_data_channel_with_data_channel_dict("unreliable", &unreliable_init);
        {
            let mut channels = peer.channels.write().unwrap_or_else(|e| e.into_inner());
            channels.insert(DeliveryMethod::Unreliable, unreliable.clone());
        }
        let room_id = self
            .room_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        Peer::setup_dc_handlers(
            reliable,
            DeliveryMethod::ReliableOrdered,
            peer.clone(),
            self.event_handler.clone(),
            node.clone(),
            room_id.clone(),
            self.connection_states.clone(),
            self.disconnected_since.clone(),
            self.peers.clone(),
            self.peer_senders.clone(),
            self.pending_candidates.clone(),
        );

        Peer::setup_dc_handlers(
            unreliable_ordered,
            DeliveryMethod::UnreliableOrdered,
            peer.clone(),
            self.event_handler.clone(),
            node.clone(),
            room_id.clone(),
            self.connection_states.clone(),
            self.disconnected_since.clone(),
            self.peers.clone(),
            self.peer_senders.clone(),
            self.pending_candidates.clone(),
        );

        Peer::setup_dc_handlers(
            unreliable,
            DeliveryMethod::Unreliable,
            peer.clone(),
            self.event_handler.clone(),
            node.clone(),
            room_id,
            self.connection_states.clone(),
            self.disconnected_since.clone(),
            self.peers.clone(),
            self.peer_senders.clone(),
            self.pending_candidates.clone(),
        );

        // Close any peer this displaces, same as `handle_offer` does at its
        // own `peers.insert` -- without this, an orphaned old
        // `RTCPeerConnection` is simply dropped from the map while its
        // handlers (installed via `Peer::setup_handlers`, which uses
        // `Closure::forget()` and so is never actually detached just because
        // the `Arc<Peer>` is no longer referenced here) are still live. If
        // that old connection's `oniceconnectionstatechange` fires later
        // (e.g. it was already failing when this `connect()` call started),
        // its handler does `peers.remove(&remote_id)` unconditionally -- which
        // would silently delete the *new*, working connection just inserted
        // below, not the dead one that fired the event.
        let old_peer = {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            peers.insert(node.clone(), peer.clone())
        };
        if let Some(old_peer) = old_peer {
            old_peer.close_all(node);
        }

        self.spawn_connection_watchdog(node.clone(), attempt_id, peer.clone());

        if let Err(e) = self.renegotiate_peer(node, &peer).await {
            self.cleanup_peer_connection(node, true, IsolationRecovery::Schedule);
            return Err(e);
        }

        Ok(())
    }

    async fn disconnect(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        self.cleanup_peer_connection(node, true, IsolationRecovery::Schedule);
        Ok(())
    }

    async fn suspect_disconnected(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        if !self.mark_suspect_disconnected(node) {
            tracing::debug!(
                "[CS] ignored suspect-disconnected for {} (not Connected, or already in grace)",
                node.0
            );
        }
        Ok(())
    }

    async fn clear_suspect(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        if !self.cancel_suspect_grace(node) {
            tracing::debug!(
                "[CS] ignored clear-suspect for {} (no liveness-suspect grace active)",
                node.0
            );
        }
        Ok(())
    }

    fn get_connected_nodes(&self) -> Vec<NodeId> {
        let states = self
            .connection_states
            .read()
            .unwrap_or_else(|e| e.into_inner());
        states
            .iter()
            .filter(|(_, &s)| s == ConnectionState::Connected)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl SignalingHandler for WasmWebRtcTransport {
    async fn handle_message(&self, msg: MessageContent) -> mistlib_core::error::Result<()> {
        let data = match msg {
            MessageContent::Data(d) => d,
            _ => return Ok(()),
        };

        let current_room_id = self
            .room_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !data.room_id.is_empty() && data.room_id != current_room_id {
            tracing::warn!(
                "WasmWebRtcTransport: ignore signaling from different room_id {} (current={})",
                data.room_id,
                current_room_id
            );
            return Ok(());
        }

        match data.signaling_type {
            SignalingType::Offer => {
                tracing::info!("Received Offer from: {}", data.sender_id.0);
                self.handle_offer(data.sender_id.clone(), data.data).await?;
            }
            SignalingType::Answer => {
                tracing::info!("Received Answer from: {}", data.sender_id.0);
                let peer = {
                    let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
                    peers.get(&data.sender_id).cloned()
                };
                if let Some(peer) = peer {
                    if peer.pc.signaling_state() != RtcSignalingState::HaveLocalOffer {
                        return Err(mistlib_core::error::MistError::Internal(format!(
                            "Answer precondition failed: signaling state is not HaveLocalOffer for {}",
                            data.sender_id.0
                        )));
                    }

                    let sdp = sdp_from_signaling_payload(&data.data);

                    // Stale-answer guard: a duplicate/late answer for a
                    // *previous* local offer can still arrive after we've
                    // already replaced that offer with a newer one (e.g. a
                    // publish/unpublish triggered a fresh renegotiation while
                    // an earlier offer's answer was still in flight). Its
                    // m-line count/order then no longer matches our current
                    // local offer, and handing it to
                    // `set_remote_description` gets it rejected by Chrome
                    // with "The order of m-lines in answer doesn't match
                    // order in offer" -- at which point the old code below
                    // would roll back to `Stable` on that failure, discarding
                    // our own still-valid in-flight offer. When the genuine
                    // answer to *that* offer then arrives, signaling is
                    // already `Stable` instead of `HaveLocalOffer`, so it's
                    // rejected too and the negotiation change (e.g. a track
                    // publish) is silently lost until some later recovery
                    // event happens to renegotiate. Comparing signatures
                    // up front catches this before it ever reaches
                    // `set_remote_description`, so the live offer's
                    // `HaveLocalOffer` state is left untouched for the
                    // genuine answer to apply to.
                    if let Some(local_desc) = peer.pc.local_description() {
                        let local_sig = mline_signature(&local_desc.sdp());
                        let answer_sig = mline_signature(&sdp);
                        if local_sig != answer_sig {
                            tracing::warn!(
                                "Ignoring stale answer from {} (m-line signature {:?} does not match local offer {:?})",
                                data.sender_id.0,
                                answer_sig,
                                local_sig
                            );
                            peer.needs_track_reconcile.store(true, Ordering::SeqCst);
                            return Ok(());
                        }
                    }

                    let sdp_init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
                    sdp_init.set_sdp(&sdp);
                    if let Err(e) = JsFuture::from(peer.pc.set_remote_description(&sdp_init)).await
                    {
                        // A malformed/rejected answer would otherwise leave
                        // this peer stuck at HaveLocalOffer forever (our own
                        // offer was already applied by `renegotiate_peer`/
                        // `trigger_ice_restart`) -- roll back to Stable, same
                        // as `renegotiate_peer`/`apply_offer_in_place` already
                        // do on their own failure paths, so this peer isn't
                        // wedged rejecting every later negotiation attempt.
                        rollback_to_stable_on_failure(&peer, &data.sender_id, &current_room_id)
                            .await;
                        return Err(mistlib_core::error::MistError::Internal(format!("{:?}", e)));
                    }
                    self.apply_pending_candidates(&data.sender_id, &peer);

                    // Offerer-side settle-point: applying the remote's answer
                    // just returned signaling to `Stable`, completing the
                    // offer round-trip `renegotiate_peer`/`trigger_ice_restart`
                    // started. Drain `needs_track_reconcile` here, same as
                    // `apply_offer_in_place` does on the answerer side, so
                    // the invariant is "every successful negotiation
                    // settlement consumes the flag". Without this drain, a
                    // publish/unpublish deferred *while our offer was in
                    // flight* (its renegotiation rejected with "signaling
                    // state is not stable") could wait forever: the
                    // ICE-Connected trigger is edge-triggered and may have
                    // already fired before the flag was set (a publish racing
                    // a peer bounce -- exactly the screen-switch-during-
                    // reconnect case), and a healthy remote may never send
                    // another offer of its own. Spawned rather than awaited
                    // so `handle_message` isn't blocked on a fresh offer
                    // round-trip (`reconcile_peer_tracks` ->
                    // `renegotiate_peer` takes `Peer::negotiating` and does
                    // its own signaling I/O).
                    if peer
                        .needs_track_reconcile
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        let remote_id = data.sender_id.clone();
                        let room_id = current_room_id.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            let Some(transport) = crate::app::session_webrtc(&room_id) else {
                                return;
                            };
                            transport.reconcile_peer_tracks(&remote_id).await;
                        });
                    }
                }
            }
            SignalingType::Candidate => {
                tracing::info!("Received Candidate from: {}", data.sender_id.0);
                self.handle_candidate_payload(&data.sender_id, data.data);
            }
            SignalingType::Candidates => {
                tracing::info!("Received Candidates from: {}", data.sender_id.0);
                if let Ok(candidates) = serde_json::from_str::<Vec<String>>(&data.data) {
                    for cand in candidates {
                        self.handle_candidate_payload(&data.sender_id, cand);
                    }
                }
            }
            SignalingType::Request => {
                tracing::info!("Received Request from: {}", data.sender_id.0);
                if self.local_node_id != data.sender_id && self.local_node_id.0 < data.sender_id.0 {
                    // Keep the same shape as native: do not tear down healthy or
                    // actively-watched sessions on Request; stale remnants are
                    // cleaned before reconnecting.
                    let (action, restarted_epoch) = self.request_action_for(&data.sender_id);
                    match action {
                        RequestAction::Ignore => {
                            tracing::info!(
                                "Ignoring Request from {} because an active session already exists",
                                data.sender_id.0
                            );
                            return Ok(());
                        }
                        RequestAction::CleanupAndConnect => {
                            tracing::info!(
                                "Resetting stale peer state before reconnect: {}",
                                data.sender_id.0
                            );
                            // Skip: `connect()` runs immediately below, so this
                            // is not a genuine isolation -- see
                            // `IsolationRecovery`'s doc.
                            self.cleanup_peer_connection(
                                &data.sender_id,
                                true,
                                IsolationRecovery::Skip,
                            );
                        }
                        RequestAction::Connect => {}
                    }

                    tracing::info!("Initiating connect to: {}", data.sender_id.0);
                    let connected = self.connect(&data.sender_id).await.is_ok();
                    // If this reconnect was restart-driven, stamp the epoch
                    // it was built for once the connection actually succeeds
                    // -- see `peer_epochs`' field doc.
                    if connected {
                        if let Some(epoch) = restarted_epoch {
                            self.peer_epochs
                                .write()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(data.sender_id.clone(), epoch);
                        }
                    }
                }
            }
            SignalingType::Rejoin => {
                // Locally synthesized by the signaling layer, never sent over
                // the wire (`SignalingType::is_local_only`): it detected that
                // `data.sender_id`'s signaling identity rebound to a fresh
                // session (browser reload, process restart) and injects this
                // immediately ahead of that peer's real Offer/Request on this
                // same ordered stream. Carries no SDP -- `data.data` is the
                // restarted peer's new `joined_at` epoch (decimal unix
                // millis, "0" if unknown) instead, since `SignalingData` has
                // too many construction sites to add a dedicated field for
                // it.
                let epoch = data.data.parse::<u64>().unwrap_or(0);

                // Epoch-idempotency guard, checked before any teardown: a
                // duplicate or reordered-late `Rejoin` can carry an epoch we
                // have already rebuilt for (see `peer_epochs`' field doc for
                // why `restarted_peers` alone isn't enough to catch this). If
                // a peer is currently held for this sender AND we recorded it
                // as built for an epoch >= this one, tearing it down would
                // destroy a connection that is already the newest session --
                // exactly the kind of self-inflicted teardown that composed
                // with `schedule_isolation_recovery` into the livelock this
                // whole restart mechanism is here to avoid (see
                // `IsolationRecovery`'s doc). `epoch == 0` means unknown (the
                // legacy/unparseable case) -- that can never prove the
                // current peer is newer, so it must NOT skip the teardown.
                if epoch != 0 {
                    let peer_exists = {
                        let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
                        peers.contains_key(&data.sender_id)
                    };
                    if peer_exists {
                        let known_epoch = self
                            .peer_epochs
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&data.sender_id)
                            .copied();
                        if let Some(known_epoch) = known_epoch {
                            if known_epoch >= epoch {
                                tracing::info!(
                                    "Ignoring Rejoin from {} (epoch={}): current peer already built for session epoch {}",
                                    data.sender_id.0,
                                    epoch,
                                    known_epoch
                                );
                                return Ok(());
                            }
                        }
                    }
                }

                tracing::info!(
                    "Received Rejoin from {} (epoch={}); tearing down stale peer state",
                    data.sender_id.0,
                    epoch
                );

                // Record the restart before tearing anything down: if the
                // real Offer/Request that follows on this same stream is
                // itself delayed or reordered relative to some other event,
                // `request_action_for`/`handle_offer` still have this to
                // consult (and consume) via `take_remote_restarted`. A
                // `Rejoin` for a peer that already has an entry -- e.g. two
                // rapid reloads -- keeps the newer of the two epochs rather
                // than stacking.
                {
                    let mut restarted = self
                        .restarted_peers
                        .write()
                        .unwrap_or_else(|e| e.into_inner());
                    restarted
                        .entry(data.sender_id.clone())
                        .and_modify(|existing| *existing = (*existing).max(epoch))
                        .or_insert(epoch);
                }

                // Tear down the stale peer synchronously (no `.await` in
                // `cleanup_peer_connection`) so the real Offer/Request right
                // behind this on the same ordered stream sees a clean slate
                // instead of racing a still-registered (if already-doomed)
                // peer connection. Same helper and `close_pc = true` as
                // `RequestAction::CleanupAndConnect` above. `IsolationRecovery::Skip`:
                // the peer's real Offer/Request is next on this same ordered
                // stream and will rebuild the connection, so this is not a
                // genuine isolation -- see `IsolationRecovery`'s doc for why
                // scheduling recovery here used to cause the livelock.
                self.cleanup_peer_connection(&data.sender_id, true, IsolationRecovery::Skip);

                // No SDP to apply and no connection to initiate here -- the
                // peer's actual Offer/Request, arriving next, does that.
            }
        }
        Ok(())
    }
}
