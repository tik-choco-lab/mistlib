use super::{DisconnectGrace, GraceOrigin};
use bytes::Bytes;
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::stats::STATS;
use mistlib_core::transport::NetworkEvent;
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock as StdRwLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, OwnedSemaphorePermit, RwLock};
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::offer_answer_options::RTCOfferOptions;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::signaling_state::RTCSignalingState;
use webrtc::peer_connection::RTCPeerConnection;

use tokio_util::sync::CancellationToken;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_remote::TrackRemote;

#[cfg(test)]
const ISOLATION_RECOVERY_DELAY_MS: u64 = 10;
#[cfg(not(test))]
const ISOLATION_RECOVERY_DELAY_MS: u64 = 3000;

/// Upper bound on not-yet-sent messages buffered per peer in its
/// [`Peer::spawn_send_queue`] drainer -- both while waiting for an in-order
/// slot behind earlier queued sends, and while the target DataChannel isn't
/// `Open` yet (e.g. mid ICE-restart, within `DISCONNECTED_GRACE_MS`). Sized
/// well above what a typical application send rate produces during one grace
/// period (5s in production) so a brief outage doesn't lose messages
/// outright, while still bounding memory for a peer that never recovers --
/// that case is reaped by the sweeper (`sweeper::decide_grace_expiry`), which
/// tears down the whole `Peer` (and this queue with it) via `close_all`.
pub(crate) const PEER_SEND_QUEUE_CAPACITY: usize = 256;

/// Poll interval [`Peer::spawn_send_queue`]'s drainer uses while waiting for
/// a queued message's target DataChannel to become `Open`. There is no
/// cheap async "wait until open" signal exposed by `webrtc-rs`'s
/// `RTCDataChannel` (only the `on_open` callback, which is already spoken
/// for by `setup_dc_open_handler`), so this mirrors the existing
/// `ready_state()`-polling idiom used elsewhere in this module (e.g. the
/// sweeper).
#[cfg(test)]
const SEND_QUEUE_POLL_INTERVAL_MS: u64 = 5;
#[cfg(not(test))]
const SEND_QUEUE_POLL_INTERVAL_MS: u64 = 20;

/// Attempts for [`send_signaling_with_retry`]'s bounded retry of a
/// fire-and-forget signaling send (ICE candidates today; see its call site in
/// `setup_ice_candidate_handler`). Small and fixed -- this is meant to ride
/// out a transient signaling-layer hiccup within the same tick, not to be a
/// long-running retry policy. If every attempt fails, the send is still
/// dropped (as it always was): nothing here changes the fire-and-forget
/// contract, it just makes a single blip far less likely to be the thing
/// that drops the message.
const SIGNALING_SEND_RETRY_ATTEMPTS: u32 = 3;
#[cfg(test)]
const SIGNALING_SEND_RETRY_BACKOFF_MS: u64 = 5;
#[cfg(not(test))]
const SIGNALING_SEND_RETRY_BACKOFF_MS: u64 = 100;

/// How many times [`PeerSharedHandles::try_ice_restart`] retries its whole
/// create-offer/apply/send sequence within one ICE-restart episode before
/// giving up and leaving the sweeper's grace-period teardown as the only
/// remaining recovery path. Kept small: this all still needs to complete
/// well inside `DISCONNECTED_GRACE_MS`, and a signaling-state precondition
/// failure that persists across every attempt (some other negotiation
/// genuinely in flight) won't be fixed by trying a fourth time either.
const ICE_RESTART_RETRY_ATTEMPTS: u32 = 3;
/// Exponential backoff schedule for [`PeerSharedHandles::try_ice_restart`]'s
/// retry loop (see `super::backoff::exponential_backoff_ms`): starts at
/// `ICE_RESTART_RETRY_INITIAL_BACKOFF_MS`, doubles each retry, capped at
/// `ICE_RESTART_RETRY_MAX_BACKOFF_MS`. Replaces the old fixed 500ms
/// interval. With `ICE_RESTART_RETRY_ATTEMPTS == 3` (2 waited intervals:
/// 500ms, 1000ms in production) the whole retry loop still finishes in ~1.5s
/// -- well inside `DISCONNECTED_GRACE_MS`'s 5s default, so the sweeper never
/// reaps a session ICE restart is still actively working on.
#[cfg(test)]
const ICE_RESTART_RETRY_INITIAL_BACKOFF_MS: u64 = 5;
#[cfg(not(test))]
const ICE_RESTART_RETRY_INITIAL_BACKOFF_MS: u64 = 500;
const ICE_RESTART_RETRY_BACKOFF_MULTIPLIER: f64 = 2.0;
#[cfg(test)]
const ICE_RESTART_RETRY_MAX_BACKOFF_MS: u64 = 20;
#[cfg(not(test))]
const ICE_RESTART_RETRY_MAX_BACKOFF_MS: u64 = 2000;

/// Bounded retry for a `send_signaling` call: tries up to
/// [`SIGNALING_SEND_RETRY_ATTEMPTS`] times with a short fixed backoff between
/// attempts, logging (`debug`) each intermediate failure and a final `warn`
/// only if every attempt failed. Returns the last error if every attempt
/// failed. Callers that only ever discarded the original one-shot `Result`
/// (`let _ = ...`) can keep doing exactly that (`let _ = send_signaling_with_retry(...).await;`)
/// -- the retry is purely an internal implementation detail of the send, not
/// a change to the fire-and-forget contract at the call site.
async fn send_signaling_with_retry(
    signaler: &Arc<dyn Signaler>,
    to: &NodeId,
    msg: &MessageContent,
    context: &str,
) -> mistlib_core::error::Result<()> {
    let mut last_err = None;
    for attempt in 1..=SIGNALING_SEND_RETRY_ATTEMPTS {
        match signaler.send_signaling(to, msg.clone()).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if attempt < SIGNALING_SEND_RETRY_ATTEMPTS {
                    tracing::debug!(
                        "[{}] send_signaling attempt {}/{} failed for {}: {}; retrying",
                        context,
                        attempt,
                        SIGNALING_SEND_RETRY_ATTEMPTS,
                        to,
                        err
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(
                        SIGNALING_SEND_RETRY_BACKOFF_MS,
                    ))
                    .await;
                } else {
                    tracing::warn!(
                        "[{}] send_signaling failed after {} attempts for {}: {}",
                        context,
                        SIGNALING_SEND_RETRY_ATTEMPTS,
                        to,
                        err
                    );
                }
                last_err = Some(err);
            }
        }
    }
    Err(last_err.expect("loop runs SIGNALING_SEND_RETRY_ATTEMPTS >= 1 times"))
}

/// Result of a single [`PeerSharedHandles::try_ice_restart_once`] attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IceRestartOutcome {
    /// The restart offer was applied locally and sent.
    Sent,
    /// No live peer for this node -- retrying would not help, nothing to
    /// restart.
    NoPeer,
    /// A transient failure (non-`Stable` signaling state, a `create_offer`/
    /// `set_local_description` error, or a signaling send that failed even
    /// after its own bounded retry) -- worth retrying the whole sequence.
    Retryable,
}

/// How long `Peer::close_all` waits, after asking each data channel to close,
/// before tearing down the underlying peer connection. See the comment at
/// its call site for why this is needed.
const CLOSE_FLUSH_DELAY_MS: u64 = 50;

/// Emitted when a remote media track (audio/video) arrives on a peer connection.
/// Consumers (e.g. mistlib-media's stream broadcaster) subscribe to these via
/// `WebRtcTransport::set_media_track_handler`.
pub struct MediaTrackEvent {
    pub remote_id: NodeId,
    pub track: Arc<TrackRemote>,
    pub receiver: Arc<RTCRtpReceiver>,
    /// The peer connection the track arrived on, so the consumer can send
    /// RTCP feedback (PLI/NACK) back to the publisher via `pc.write_rtcp`.
    pub pc: Arc<RTCPeerConnection>,
}

pub struct Peer {
    pub pc: Arc<RTCPeerConnection>,
    pub channels: Arc<RwLock<HashMap<DeliveryMethod, Arc<RTCDataChannel>>>>,
    pub cancel_token: CancellationToken,
    /// Set when our local offer was applied (signaling state advanced to
    /// `HaveLocalOffer`) but the signaling send failed, so the remote never
    /// received it (e.g. `RoutedSignaler` returning `RouteNotFound` for a
    /// just-established connection whose overlay route hasn't caught up yet).
    ///
    /// webrtc-rs 0.13 implements NO rollback transitions at all --
    /// `check_next_signaling_state` (webrtc's `signaling_state.rs`) has no
    /// `Rollback` arm out of any non-`Stable` state, so
    /// `rollback_to_stable_on_failure` can never actually restore `Stable`
    /// for this case (it's kept as best-effort for a future webrtc that does
    /// support it). The working recovery contract is instead: `send_offer`
    /// may re-offer from `HaveLocalOffer` when (and only when) this flag says
    /// the pending local offer never left this machine --
    /// `HaveLocalOffer -> SetLocal(offer) -> HaveLocalOffer` is a valid
    /// webrtc-rs transition, and since the lost offer was never delivered, no
    /// answer to it can ever arrive to mismatch the re-offer. Cleared on the
    /// next successful offer send. Not set for offers that were delivered:
    /// re-offering over a genuinely in-flight offer stays forbidden
    /// (`can_send_offer`'s `Stable` precondition), because the remote's
    /// answer to the first offer could then land on the replaced one.
    pub local_offer_unsent: std::sync::atomic::AtomicBool,
    /// Serializes this peer's negotiation steps -- `WebRtcTransport::send_offer`
    /// and `signaling::apply_offer` each take this for their *entire*
    /// create/apply(+send) sequence, not just the precondition check. Mirrors
    /// `mistlib-wasm`'s `Peer::negotiating` (see its doc comment there for the
    /// original rationale); native needs the same guard for a related but
    /// distinct reason: unlike the WebSocket-bootstrap signaling path (a
    /// single sequential consumer loop in `MistEngine::run`, one message
    /// awaited fully before the next is read), signaling delivered once this
    /// peer's connection has joined the overlay mesh goes through
    /// `MistEngine::handle_message_content`
    /// (`mistlib-core/src/engine/network.rs`), which `tokio::spawn`s a brand
    /// new, unserialized task per inbound message. Two such messages for the
    /// same peer arriving close together (e.g. a browser sending a
    /// track-publish offer immediately followed by a reconcile offer) are
    /// then genuinely concurrent `apply_offer` invocations with nothing else
    /// serializing them: both can read `Stable` off the shared
    /// `RTCPeerConnection` before either mutates it, then interleave their
    /// `set_remote_description`/`create_answer`/`set_local_description` calls
    /// against each other. Whichever call's tail-end `local_description()`
    /// read loses the race sends an answer that doesn't match the offer the
    /// far side thinks it just received (wrong m-line count/order), which
    /// Chrome rejects with "The order of m-lines in answer doesn't match
    /// order in offer". Holding this lock for the whole sequence -- exactly
    /// like `send_offer`'s own comment on `Peer::local_offer_unsent`
    /// describes for the lost-send-retry case -- makes the second caller wait
    /// its turn and re-observe the *actual* current state instead of acting
    /// on a stale snapshot.
    pub negotiating: tokio::sync::Mutex<()>,
    /// Sends a message into this peer's ordered send queue -- see
    /// `spawn_send_queue`'s doc comment for why this exists.
    /// `WebRtcTransport::send` (`transports/webrtc.rs`) is the only producer.
    pub(crate) send_tx: mpsc::Sender<QueuedSend>,
}

/// One not-yet-sent message waiting in a [`Peer`]'s send queue.
pub(crate) struct QueuedSend {
    pub data: Bytes,
    pub method: DeliveryMethod,
}

/// Shared transport-level state passed into peer handler setup functions.
#[derive(Clone)]
pub struct PeerSharedHandles {
    pub connection_states: Arc<StdRwLock<HashMap<NodeId, ConnectionState>>>,
    pub peers: Arc<RwLock<HashMap<NodeId, Arc<Peer>>>>,
    /// Sync mirror of `peers`' keyset -- see
    /// `WebRtcTransport::send_queues`'s doc comment. Threaded through here
    /// because `cleanup_session_impl`/`remove_peer_if_current` and the
    /// `Failed`/`Closed` and data-channel-close teardown paths below are
    /// removal sites that must stay in lock-step with `peers`.
    pub(crate) send_queues: Arc<StdRwLock<HashMap<NodeId, mpsc::Sender<QueuedSend>>>>,
    pub pending_candidates: Arc<RwLock<HashMap<NodeId, Vec<String>>>>,
    pub connection_attempt_ids: Arc<StdRwLock<HashMap<NodeId, u32>>>,
    pub connect_request_attempt_ids: Arc<StdRwLock<HashMap<NodeId, u32>>>,
    /// Sweeper livelock fix -- see `WebRtcTransport::connecting_reserved_at`'s
    /// doc comment.
    pub connecting_reserved_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    pub pc_connected_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    /// Remote-takeover fix -- see `WebRtcTransport::established_at`'s doc
    /// comment.
    pub established_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    pub handshake_permits: Arc<StdRwLock<HashMap<NodeId, OwnedSemaphorePermit>>>,
    pub last_disconnect_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    /// Repair-first ICE restart -- see `WebRtcTransport::last_ice_restart_at`'s
    /// doc comment.
    pub last_ice_restart_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    pub disconnected_since: Arc<StdRwLock<HashMap<NodeId, DisconnectGrace>>>,
    /// `[ConnTiming]` instrumentation -- see
    /// `WebRtcTransport::connect_started_at`'s doc comment.
    pub connect_started_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    /// `[ConnTiming]` instrumentation -- see
    /// `WebRtcTransport::disconnect_observed_at`'s doc comment.
    pub disconnect_observed_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    /// Buffer-don't-drop fix -- see
    /// `WebRtcTransport::pending_candidates_first_seen`'s doc comment.
    pub pending_candidates_first_seen: Arc<RwLock<HashMap<NodeId, Instant>>>,
    pub signaler: Arc<dyn Signaler>,
    pub isolation_recovery_epoch: Arc<AtomicU64>,
    /// The room this transport belongs to (SPEC-15): each session owns its
    /// own `WebRtcTransport`, so this never changes over these handles'
    /// lifetime. Used to tag connect/disconnect notifications with the right
    /// room instead of an ambiguous "current" session.
    pub room_id: String,
    /// This transport's own node ID. Used by `try_ice_restart` both to
    /// determine ICE-restart initiator direction (`is_ice_restart_initiator`)
    /// and to stamp the restart offer's `sender_id`.
    pub local_node_id: NodeId,
}

impl PeerSharedHandles {
    /// Shared by the ICE `Disconnected` handler and `mark_suspect_disconnected`:
    /// moves a reserved peer into `Reconnecting` and starts (or leaves alone,
    /// if one is already running) its grace-period clock, tagged with `origin`.
    /// Returns `(reserved, freshly_started)`: `reserved` is `false` if `node`
    /// wasn't in `connection_states` at all (nothing to do, as before);
    /// `freshly_started` is `true` only when this call actually created the
    /// grace entry -- a repeat call while a grace period is already running
    /// leaves the original `started_at`/`origin` untouched and reports `false`.
    ///
    /// Repair-first ICE restart: a freshly-started grace, regardless of
    /// `origin`, also fires the appropriate repair trigger via
    /// `spawn_repair_trigger` -- both the ICE `Disconnected` state-change arm
    /// (`origin: Ice`) and a freshly-suspected liveness failure (`origin:
    /// LivenessSuspect`) are equally good reasons to attempt the same cheap,
    /// non-destructive repair, so this is centralized here instead of
    /// duplicated at each of `start_disconnect_grace`'s two call sites.
    fn start_disconnect_grace(&self, node: &NodeId, origin: GraceOrigin) -> (bool, bool) {
        let mut states = self.connection_states.write().unwrap();
        if !states.contains_key(node) {
            return (false, false);
        }
        states.insert(node.clone(), ConnectionState::Reconnecting);
        let started_now = {
            let mut disconnected = self.disconnected_since.write().unwrap();
            match disconnected.entry(node.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(DisconnectGrace {
                        started_at: Instant::now(),
                        origin,
                    });
                    true
                }
                std::collections::hash_map::Entry::Occupied(_) => false,
            }
        };
        tracing::warn!(
            "[CS] Disconnected (grace started={}, origin={:?}): {} total={}",
            started_now,
            origin,
            node,
            states.len()
        );
        if started_now {
            self.spawn_repair_trigger(node, origin);
        }
        (true, started_now)
    }

    /// Repair-first ICE restart: fires the appropriate repair trigger for a
    /// grace period that just began, based on which side of the
    /// deterministic initiator/non-initiator split (`is_ice_restart_initiator`)
    /// `self.local_node_id` is on for `node`. The initiator attempts the
    /// actual (non-destructive) ICE restart itself, subject to the per-peer
    /// rate limit (`maybe_try_ice_restart`); the non-initiator has no
    /// PC-level trigger of its own -- only the initiator ever calls
    /// `create_offer(ice_restart: true)` for this pair -- so it sends a
    /// lightweight `RestartRequest` nudge instead, asking the initiator to
    /// try (`send_restart_request`).
    ///
    /// Both are fire-and-forget `tokio::spawn`s: `self` (`PeerSharedHandles`)
    /// is cheap to clone into the spawned task, and every caller of
    /// `start_disconnect_grace` (including the synchronous
    /// `on_peer_connection_state_change` callback) cannot `.await` directly.
    ///
    /// Storm-avoidance fix: both branches first run their action through
    /// `debounce_repair_trigger`, which sleeps
    /// `super::REPAIR_TRIGGER_DEBOUNCE_MS` (plus per-pair jitter) and
    /// re-checks that the grace is still pending before proceeding -- see
    /// that method's doc comment and `super::REPAIR_TRIGGER_DEBOUNCE_MS`'s
    /// for the measured repair-storm regression this closes.
    fn spawn_repair_trigger(&self, node: &NodeId, origin: GraceOrigin) {
        let handles = self.clone();
        let node = node.clone();
        if super::is_ice_restart_initiator(&self.local_node_id, &node) {
            let restart_origin = match origin {
                GraceOrigin::Ice => "ice_state",
                GraceOrigin::LivenessSuspect => "liveness_suspect",
            };
            tokio::spawn(async move {
                if handles.debounce_repair_trigger(&node).await {
                    handles.maybe_try_ice_restart(&node, restart_origin).await;
                }
            });
        } else {
            tokio::spawn(async move {
                if handles.debounce_repair_trigger(&node).await {
                    handles.send_restart_request(&node).await;
                }
            });
        }
    }

    /// Repair-first ICE restart, storm-avoidance fix: sleeps
    /// `super::REPAIR_TRIGGER_DEBOUNCE_MS` plus a deterministic per-pair
    /// jitter (`super::repair_trigger_jitter_ms`, bounded by
    /// `super::REPAIR_TRIGGER_JITTER_MS`) before either spawned branch of
    /// `spawn_repair_trigger` is allowed to act, then re-checks that `node`'s
    /// disconnect grace (`disconnected_since`) is STILL present. Returns
    /// `true` iff the grace survived the debounce window and the caller
    /// should proceed with its repair action; `false` means the session
    /// already recovered on its own during the wait (e.g. the wake-race
    /// backlog was processed, `recover_connected_from_grace` fired, or a
    /// liveness suspect was `clear_suspect`d by a PONG) and the repair action
    /// must be skipped entirely.
    ///
    /// Checking `disconnected_since` alone is sufficient -- no extra PC-state
    /// probe is needed -- because every recovery path removes the entry (see
    /// `mark_connection_state`'s `Connected` arm) while a still-broken
    /// session always keeps it. See `super::REPAIR_TRIGGER_DEBOUNCE_MS`'s doc
    /// comment for the measured repair-storm regression (a resumed-from-
    /// freeze process spuriously detecting all peers as `Disconnected`
    /// before draining its socket backlog) this closes.
    async fn debounce_repair_trigger(&self, node: &NodeId) -> bool {
        let jitter_ms = super::repair_trigger_jitter_ms(&self.local_node_id, node);
        let delay_ms = super::REPAIR_TRIGGER_DEBOUNCE_MS + jitter_ms;
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let still_pending = self.disconnected_since.read().unwrap().contains_key(node);
        if !still_pending {
            tracing::debug!(
                "[IceRestart] debounce-skip {}: grace cleared during {}ms debounce+jitter \
                 window (self-recovered)",
                node,
                delay_ms
            );
        }
        still_pending
    }

    /// Returns `(reserved, freshly_started)` -- see `start_disconnect_grace`,
    /// which also fires the repair-trigger side effect for a freshly-started
    /// grace.
    pub(crate) fn mark_disconnected_grace(&self, node: &NodeId) -> (bool, bool) {
        self.start_disconnect_grace(node, GraceOrigin::Ice)
    }

    /// Entry point for `OverlayAction::SuspectDisconnected`: only transitions a
    /// currently-`Connected` peer into the grace flow, tagged as
    /// liveness-suspect-originated. A peer that isn't `Connected` (already
    /// reconnecting, disconnected, or unknown) is left untouched -- there's
    /// either already a grace period running (whatever its origin) or nothing
    /// to suspect in the first place. Repair-first ICE restart: a freshly-
    /// started grace also fires a repair trigger (see
    /// `start_disconnect_grace`/`spawn_repair_trigger`) -- app-level liveness
    /// suspicion used to only ever start a grace and wait, never attempt
    /// repair.
    pub(crate) fn mark_suspect_disconnected(&self, node: &NodeId) -> bool {
        {
            let states = self.connection_states.read().unwrap();
            if states.get(node) != Some(&ConnectionState::Connected) {
                return false;
            }
        }
        self.start_disconnect_grace(node, GraceOrigin::LivenessSuspect)
            .0
    }

    /// Entry point for `OverlayAction::ClearSuspect`: cancels the current grace
    /// period only if it was started by `mark_suspect_disconnected`. A grace
    /// period started by ICE `Disconnected` is left alone -- only ICE's own
    /// recovery signal is allowed to end that one.
    pub(crate) fn clear_suspect(&self, node: &NodeId) -> bool {
        let is_suspect_origin = {
            let disconnected = self.disconnected_since.read().unwrap();
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
        self.mark_connection_state(node, ConnectionState::Connected)
    }

    pub(crate) fn mark_connection_state(&self, node: &NodeId, state: ConnectionState) -> bool {
        let mut states = self.connection_states.write().unwrap();
        if !states.contains_key(node) {
            return false;
        }
        states.insert(node.clone(), state);
        if state == ConnectionState::Connected {
            let recovered = self
                .disconnected_since
                .write()
                .unwrap()
                .remove(node)
                .is_some();
            if recovered {
                tracing::info!(
                    "[CS] recovered from disconnected grace: {} total={}",
                    node,
                    states.len()
                );
            }
        }
        true
    }

    /// Flips `node` back to `Connected` iff a disconnect grace is currently
    /// pending -- the recovery half of the ICE-restart flow. After a
    /// successful restart the same `RTCPeerConnection` re-enters `Connected`
    /// while its data channels stayed open the whole time, so the
    /// ReliableOrdered DC's `on_open` (where `Connected` is normally set)
    /// never re-fires: webrtc-rs consumes that handler on its first
    /// invocation. Without this, the state would sit in `Reconnecting` until
    /// the sweeper tears the just-recovered connection down at grace expiry.
    ///
    /// Gated on the pending grace entry so a *fresh* connect keeps the
    /// "Connecting until the ReliableOrdered data channel opens" rule (the
    /// zombie-session detector) intact. If a restart recovered ICE but the
    /// data channel is genuinely dead, the sweeper's `pc_connected_at`-based
    /// zombie check -- re-armed by the `Connected` arm right before this is
    /// called -- still reaps the session.
    pub(crate) fn recover_connected_from_grace(&self, node: &NodeId) -> bool {
        if !self.disconnected_since.read().unwrap().contains_key(node) {
            return false;
        }
        self.mark_connection_state(node, ConnectionState::Connected)
    }

    /// Repair-first ICE restart: rate-limited entry point for an ICE-restart
    /// repair attempt, shared by all three trigger sites --
    /// `spawn_repair_trigger`'s initiator branch (ICE `Disconnected` state
    /// change and a freshly-started `LivenessSuspect` grace, both routed
    /// through `start_disconnect_grace`) and `WebRtcTransport::handle_restart_request`
    /// (an incoming `RestartRequest` from the non-initiator side). `origin` is
    /// used only for `[IceRestart]` observability, not for any difference in
    /// gating behavior -- the rate limit (`super::ice_restart_allowed`,
    /// `super::ICE_RESTART_MIN_INTERVAL_MS`) applies uniformly regardless of
    /// which signal triggered it.
    ///
    /// The timestamp backing the rate limit (`last_ice_restart_at`) is
    /// recorded here, immediately before actually calling `try_ice_restart`
    /// -- i.e. when an attempt actually starts, not when it succeeds and not
    /// on a rate-limited skip. `try_ice_restart` itself can still end up
    /// doing nothing (e.g. `IceRestartOutcome::NoPeer`); that's fine, this
    /// gate only needs to bound how often the whole attempt sequence is
    /// *tried* for a given peer, not condition it on success.
    ///
    /// ICE restart as rescue, not reflex: the same admission point also
    /// re-arms `node`'s disconnect grace (`rearm_disconnect_grace`), if one is
    /// currently running -- see that method's doc comment for why an admitted
    /// attempt needs a fresh grace window rather than racing the sweeper with
    /// a clock that had already been running since the original failure was
    /// detected.
    ///
    /// Degraded-transport gate: before any of the above, this also refuses to
    /// act on a peer whose `RTCPeerConnection` is still (or already back to)
    /// `Connected` -- see the in-body comment for the fault-injection evidence
    /// that a restart offer applied to a healthy PC wipes the answerer's
    /// candidate pairs and kills the session rather than being a harmless
    /// renegotiation. This gate is checked first, ahead of the rate limit, and
    /// applies uniformly to all three trigger sites regardless of `origin`.
    pub(crate) async fn maybe_try_ice_restart(&self, node: &NodeId, origin: &'static str) {
        // Restart only a transport that is actually degraded. Firing an ICE
        // restart at a PC that is still `Connected` is NOT a harmless
        // renegotiation in webrtc-rs: fault-injection tracing (4s cross-host
        // UDP drop) showed the offerer's agent keeps riding its still-valid
        // old candidate pair while the answerer, applying the restart offer,
        // wipes its pairs and then sits at "pingAllCandidates called with no
        // candidate pairs" until its connect watchdog kills the session. A
        // healthy-looking PC therefore must never be restarted from a
        // liveness hunch (missed PONGs on the best-effort channel) or a
        // stale RestartRequest -- the sweeper's false-positive recovery and
        // the app-level grace machinery already handle those. A genuinely
        // broken transport (NAT rebind, path change) reports Disconnected/
        // Failed here and passes this gate.
        let pc_connected = {
            let peers = self.peers.read().await;
            peers
                .get(node)
                .map(|peer| peer.pc.connection_state() == RTCPeerConnectionState::Connected)
        };
        match pc_connected {
            None => {
                tracing::debug!(
                    "[IceRestart] skip {} (origin={}): no live peer to restart",
                    node,
                    origin
                );
                return;
            }
            Some(true) => {
                tracing::debug!(
                    "[IceRestart] skip {} (origin={}): pc still Connected -- restart is for \
                     degraded transports only",
                    node,
                    origin
                );
                return;
            }
            Some(false) => {}
        }
        let ms_since_last = self
            .last_ice_restart_at
            .read()
            .unwrap()
            .get(node)
            .map(|at| at.elapsed().as_millis() as u64);
        if !super::ice_restart_allowed(ms_since_last) {
            tracing::debug!(
                "[IceRestart] skip {} (origin={}): rate-limited (last attempt {:?}ms ago, min \
                 interval {}ms)",
                node,
                origin,
                ms_since_last,
                super::ICE_RESTART_MIN_INTERVAL_MS
            );
            return;
        }
        self.last_ice_restart_at
            .write()
            .unwrap()
            .insert(node.clone(), Instant::now());
        self.rearm_disconnect_grace(node);
        tracing::info!("[IceRestart] triggered for {} (origin={})", node, origin);
        self.try_ice_restart(node).await;
    }

    /// ICE restart as rescue, not reflex: gives an admitted repair attempt a
    /// fresh `super::DISCONNECTED_GRACE_MS` window to complete. Called from
    /// `maybe_try_ice_restart` at the moment an attempt is actually admitted
    /// (passed the rate limit) -- if a disconnect grace for `node` is
    /// currently running, its `started_at` is reset to `Instant::now()` (the
    /// `origin` is left unchanged: this never turns an `Ice`-origin grace
    /// into a `LivenessSuspect` one or vice versa). A no-op if no grace is
    /// running for `node` (e.g. the restart was triggered before any grace
    /// started, or the peer was never in a grace in the first place).
    ///
    /// Without this, a restart admitted late in a grace's lifetime -- exactly
    /// the case this repair-first redesign now produces on purpose, since
    /// `super::REPAIR_TRIGGER_DEBOUNCE_MS`'s widening deliberately delays a
    /// restart's first opportunity to fire until well into the grace window
    /// -- would race `sweeper::decide_grace_expiry` using a clock that had
    /// already been running since the grace began, not since the repair
    /// attempt itself started. Measured in the 4s SIGSTOP/iptables fault
    /// injections this rescue redesign responds to: the side that fired ~200
    /// ICE restarts near the end of their graces saw 127 of those graces
    /// expire mid-handshake (`sweeper_disconnected_grace_expired`) before the
    /// restart could complete, tearing down sessions the restart itself was
    /// actively repairing.
    ///
    /// This intentionally delays teardown of a truly-dead peer via this
    /// specific path by up to one extra grace period -- remote-takeover
    /// (`should_takeover_on_fresh_offer`/`maybe_takeover_for_connect_request`)
    /// and the connect-timeout watchdog remain the fast, independent paths
    /// for a peer that is actually gone rather than merely slow to
    /// reconverge.
    fn rearm_disconnect_grace(&self, node: &NodeId) {
        if let Some(grace) = self.disconnected_since.write().unwrap().get_mut(node) {
            grace.started_at = Instant::now();
        }
    }

    /// Repair-first ICE restart, Change 3: sends a lightweight
    /// `RestartRequest` nudge to `node` when we are the non-initiator side of
    /// a freshly-started disconnect grace and therefore have no PC-level
    /// repair trigger of our own (see `spawn_repair_trigger`). Fire-and-
    /// forget: exactly one send attempt, no retry loop of its own -- the
    /// initiator's own local ICE-`Disconnected` detection and the sweeper's
    /// grace-period teardown remain the fallbacks if this is lost. Reuses the
    /// existing `SignalingType::Request` wire message tagged with
    /// `super::RESTART_REQUEST_MARKER` (see its doc comment for why this
    /// isn't a new `SignalingType` variant) rather than a bounded-retry
    /// helper like `send_signaling_with_retry`, matching the "no retry loop"
    /// intent: a lost nudge should not itself generate more signaling
    /// traffic.
    pub(crate) async fn send_restart_request(&self, node: &NodeId) {
        let msg = MessageContent::Data(SignalingData {
            sender_id: self.local_node_id.clone(),
            receiver_id: node.clone(),
            room_id: self.room_id.clone(),
            data: super::RESTART_REQUEST_MARKER.to_string(),
            signaling_type: SignalingType::Request,
        });
        match self.signaler.send_signaling(node, msg).await {
            Ok(()) => {
                tracing::info!("[IceRestart] sent RestartRequest to {}", node);
            }
            Err(err) => {
                tracing::warn!(
                    "[IceRestart] failed to send RestartRequest to {}: {}",
                    node,
                    err
                );
            }
        }
    }

    /// ICE restart attempt for `node`'s existing `RTCPeerConnection`. Called
    /// through the rate-limited `maybe_try_ice_restart` by every trigger site
    /// -- only the initiator side (`super::is_ice_restart_initiator`) ever
    /// calls this; the other side sends a `RestartRequest` nudge instead and
    /// lets the ordinary `apply_offer` (Stable -> in-place renegotiation,
    /// `signaling.rs`) path handle the resulting offer.
    ///
    /// Retries the whole create-offer/apply/send sequence
    /// ([`try_ice_restart_once`]) up to [`ICE_RESTART_RETRY_ATTEMPTS`] times
    /// with a short backoff -- a single transient failure (a `create_offer`
    /// hiccup, a signaling send that briefly can't route) used to mean giving
    /// up immediately and waiting out the entire `DISCONNECTED_GRACE_MS` for
    /// the sweeper's teardown-and-redial instead. The one outcome that is
    /// never retried is "no live peer" (`IceRestartOutcome::NoPeer`): if the
    /// peer is gone there is nothing left to restart, and no amount of
    /// retrying changes that.
    ///
    /// If every attempt fails, this is logged and dropped: the grace-period
    /// sweeper's full teardown-and-redial (`DISCONNECTED_GRACE_MS`) remains
    /// the final safety net.
    pub(crate) async fn try_ice_restart(&self, node: &NodeId) {
        for attempt in 1..=ICE_RESTART_RETRY_ATTEMPTS {
            match self.try_ice_restart_once(node).await {
                IceRestartOutcome::Sent | IceRestartOutcome::NoPeer => return,
                IceRestartOutcome::Retryable => {
                    if attempt < ICE_RESTART_RETRY_ATTEMPTS {
                        let backoff_ms = super::backoff::exponential_backoff_ms(
                            attempt,
                            ICE_RESTART_RETRY_INITIAL_BACKOFF_MS,
                            ICE_RESTART_RETRY_BACKOFF_MULTIPLIER,
                            ICE_RESTART_RETRY_MAX_BACKOFF_MS,
                        );
                        tracing::debug!(
                            "[IceRestart] attempt {}/{} failed for {}; retrying in {}ms",
                            attempt,
                            ICE_RESTART_RETRY_ATTEMPTS,
                            node,
                            backoff_ms
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    }
                }
            }
        }
        tracing::warn!(
            "[IceRestart] all {} attempts failed for {}; the sweeper's grace-period teardown is now the only remaining recovery path",
            ICE_RESTART_RETRY_ATTEMPTS,
            node
        );
    }

    /// Single create-offer/apply/send attempt backing `try_ice_restart`'s
    /// retry loop. Deliberately bypasses `WebRtcTransport::can_send_offer`'s
    /// Disconnected-reject guard: that guard protects a *healthy* connection
    /// from a stray offer, but here the entire point is to recover a
    /// connection that's already in trouble, so the same guard would just
    /// block the recovery it's meant to enable.
    async fn try_ice_restart_once(&self, node: &NodeId) -> IceRestartOutcome {
        let peer = {
            let peers = self.peers.read().await;
            peers.get(node).cloned()
        };
        let Some(peer) = peer else {
            tracing::debug!("[IceRestart] skip {}: no active peer", node);
            return IceRestartOutcome::NoPeer;
        };

        let signaling_state = peer.pc.signaling_state();
        if signaling_state != RTCSignalingState::Stable {
            tracing::debug!(
                "[IceRestart] skip {}: signaling_state={:?}",
                node,
                signaling_state
            );
            return IceRestartOutcome::Retryable;
        }

        let offer = match peer
            .pc
            .create_offer(Some(RTCOfferOptions {
                ice_restart: true,
                ..Default::default()
            }))
            .await
        {
            Ok(offer) => offer,
            Err(err) => {
                tracing::warn!("[IceRestart] create_offer failed for {}: {}", node, err);
                return IceRestartOutcome::Retryable;
            }
        };

        if let Err(err) = peer.pc.set_local_description(offer).await {
            tracing::warn!(
                "[IceRestart] set_local_description failed for {}: {}",
                node,
                err
            );
            return IceRestartOutcome::Retryable;
        }

        let Some(offer_desc) = peer.pc.local_description().await else {
            tracing::warn!("[IceRestart] no local_description after set for {}", node);
            return IceRestartOutcome::Retryable;
        };

        let msg = MessageContent::Data(SignalingData {
            sender_id: self.local_node_id.clone(),
            receiver_id: node.clone(),
            room_id: self.room_id.clone(),
            data: offer_desc.sdp,
            signaling_type: SignalingType::Offer,
        });
        // The send itself already gets its own short bounded retry (a
        // transient route-not-found/network blip shouldn't force a whole new
        // offer to be created) -- only fall back to this function's own
        // retry-from-scratch if every one of those attempts still failed.
        if send_signaling_with_retry(&self.signaler, node, &msg, "IceRestart")
            .await
            .is_err()
        {
            return IceRestartOutcome::Retryable;
        }

        tracing::info!("[IceRestart] sent restart offer to {}", node);
        IceRestartOutcome::Sent
    }

    pub async fn cleanup_session(&self, node: &NodeId, force_failed: bool) {
        self.cleanup_session_with_reason(node, force_failed, "cleanup_session")
            .await;
    }

    /// Unconditional teardown of `node`'s bookkeeping, regardless of which
    /// `Peer` (if any) currently sits behind it. Only safe for callers that
    /// genuinely want "whatever is registered for this NodeId right now,
    /// gone" -- an explicit user-initiated `disconnect()`, or a caller that
    /// doesn't hold a specific `Peer` snapshot to compare against. Callers
    /// that decided to clean up based on an earlier snapshot of a specific
    /// `Peer` (the connect-timeout watchdog, the periodic sweeper) must use
    /// `cleanup_session_if_current` instead -- see its doc comment for the
    /// race this guards against.
    pub async fn cleanup_session_with_reason(
        &self,
        node: &NodeId,
        force_failed: bool,
        reason: &'static str,
    ) {
        self.cleanup_session_impl(node, force_failed, reason, None)
            .await;
    }

    /// Same as `cleanup_session_with_reason`, but only tears down `node`'s
    /// bookkeeping if `self.peers` still maps it to `expected`.
    ///
    /// The connect-timeout watchdog and the periodic sweeper both read a
    /// `Peer` snapshot (and its live pc/dc state), decide -- based on that
    /// now-slightly-stale read -- that the session is dead, and only then
    /// call into cleanup. Between that read and this call, a concurrent
    /// reconnect for the same `NodeId` (a fresh `handle_offer` or
    /// `connect_inner`) can have already replaced `self.peers[node]` with a
    /// brand-new, healthy `Peer` and marked `connection_states[node]`
    /// `Connected`. The old, unconditional `peers.remove(node)` /
    /// `connection_states.remove(node)` pair had no way to notice this: it
    /// deleted whatever was *currently* registered, silently discarding the
    /// new peer's live registration while leaving nothing to ever
    /// re-register it (the overlay still believes the node is connected, so
    /// no new `Connect` action is generated) -- a permanent "Node not found"
    /// on every future overlay send to that peer, with no
    /// disconnect/state-change log to explain it, since nothing else in this
    /// module resurrects a `connection_states` entry without first creating
    /// a fresh `Peer`.
    ///
    /// This mirrors `remove_peer_if_current`, which already closes this
    /// exact race for the `Failed`/`Closed` peer-connection-state handler
    /// and the data-channel close handler; this variant closes it for the
    /// remaining two teardown paths (watchdog, sweeper) that still removed
    /// unconditionally.
    ///
    /// Returns `false` (nothing touched) when `expected` no longer matches
    /// what `self.peers` holds for `node` -- callers can use this to skip
    /// any of their own follow-up bookkeeping (e.g.
    /// `WebRtcTransport::published_senders`) that would otherwise wrongly
    /// touch the superseding peer's state.
    pub async fn cleanup_session_if_current(
        &self,
        node: &NodeId,
        expected: &Weak<Peer>,
        force_failed: bool,
        reason: &'static str,
    ) -> bool {
        self.cleanup_session_impl(node, force_failed, reason, Some(expected))
            .await
    }

    async fn cleanup_session_impl(
        &self,
        node: &NodeId,
        force_failed: bool,
        reason: &'static str,
        expected: Option<&Weak<Peer>>,
    ) -> bool {
        // Resolve (and, for the guarded case, atomically identity-check) the
        // `self.peers` removal FIRST, before touching any other map. For the
        // guarded case, bail out entirely -- untouched -- if `node` no
        // longer maps to `expected`: some other, more current attempt owns
        // this `NodeId`'s bookkeeping now, and every map below must be left
        // to it.
        let peer = match expected {
            Some(expected) => {
                match remove_peer_if_current(&self.peers, &self.send_queues, node, expected).await {
                    Some(peer) => Some(peer),
                    None => return false,
                }
            }
            None => {
                let mut peers = self.peers.write().await;
                let removed = peers.remove(node);
                drop(peers);
                self.send_queues.write().unwrap().remove(node);
                removed
            }
        };

        let had_attempt = {
            let mut attempts = self.connection_attempt_ids.write().unwrap();
            attempts.remove(node).is_some()
        };
        {
            // Sweeper livelock fix -- see `connecting_reserved_at`'s doc
            // comment: an attempt being torn down here, however it got here,
            // no longer needs its reservation timestamp, and a later fresh
            // reservation for the same node always overwrites this anyway.
            self.connecting_reserved_at.write().unwrap().remove(node);
        }
        let had_request = {
            let mut attempts = self.connect_request_attempt_ids.write().unwrap();
            attempts.remove(node).is_some()
        };
        let had_state = {
            let mut states = self.connection_states.write().unwrap();
            states.remove(node).is_some()
        };
        {
            let mut pc_connected = self.pc_connected_at.write().unwrap();
            pc_connected.remove(node);
        }
        {
            // Remote-takeover fix -- see `WebRtcTransport::established_at`'s
            // doc comment for why this is cleared alongside `pc_connected_at`
            // rather than swept on its own TTL.
            let mut established = self.established_at.write().unwrap();
            established.remove(node);
        }
        {
            let mut permits = self.handshake_permits.write().unwrap();
            permits.remove(node);
        }
        {
            let mut disconnected = self.disconnected_since.write().unwrap();
            disconnected.remove(node);
        }
        {
            // `[ConnTiming]` instrumentation: an attempt that is being torn
            // down here can never reach establishment, so drop its
            // attempt-start entry -- otherwise a later, unrelated attempt for
            // the same node (a fresh `connect_started_at` insert would
            // normally overwrite it, but not every teardown path is
            // guaranteed to race a fresh attempt) could leak it forever.
            self.connect_started_at.write().unwrap().remove(node);
        }
        let had_pending_candidates = {
            let mut pc_lock = self.pending_candidates.write().await;
            pc_lock.remove(node).is_some()
        };
        {
            // Buffer-don't-drop fix -- see `pending_candidates_first_seen`'s
            // doc comment: this attempt's buffer (if any) is gone now, so
            // its age no longer needs tracking either.
            self.pending_candidates_first_seen
                .write()
                .await
                .remove(node);
        }

        let had_peer = peer.is_some();
        let had_session_state =
            had_attempt || had_request || had_state || had_peer || had_pending_candidates;
        if had_session_state {
            let now = std::time::Instant::now();
            let mut last_disconnect = self.last_disconnect_at.write().unwrap();
            last_disconnect.insert(node.clone(), now);
            drop(last_disconnect);
            // `[ConnTiming]` instrumentation: see
            // `WebRtcTransport::disconnect_observed_at`'s doc comment for why
            // this is kept separately from (and longer than)
            // `last_disconnect_at`.
            let mut disconnect_observed = self.disconnect_observed_at.write().unwrap();
            super::conn_timing::insert_disconnect_observed(
                &mut disconnect_observed,
                node.clone(),
                now,
            );
        }

        if let Some(peer) = peer {
            tracing::warn!(
                "[WebRTC Close] reason={} node={} force_failed={} had_attempt={} had_request={} had_state={} had_pending_candidates={}",
                reason,
                node,
                force_failed,
                had_attempt,
                had_request,
                had_state,
                had_pending_candidates
            );
            peer.close_all().await;
            crate::mem::record_peer_cleaned();
        }

        if had_session_state {
            // `[ConnTiming]` instrumentation: one `kind=disconnect` line per
            // confirmed disconnect, gated on the same `had_session_state`
            // check that gates `on_disconnected_internal` below -- this is
            // the funnel point for every teardown path that goes through
            // `cleanup_session_with_reason`/`cleanup_session_if_current`
            // (explicit disconnect, the connect-timeout watchdog, and the
            // periodic sweeper's three reap reasons). The remaining
            // disconnect path -- the `Failed`/`Closed` peer-connection-state
            // handler below, which bypasses this function entirely -- emits
            // its own `kind=disconnect` at its own analogous
            // `had_state`-gated point, so the two never double-fire for the
            // same disconnect (whichever path's identity-guarded removal
            // wins is the only one that observes `had_state`/`had_session_state`
            // as `true`).
            super::conn_timing::log_disconnect(node, reason);
            crate::events::on_disconnected_internal(self.room_id.clone(), node.clone());
            self.schedule_isolation_recovery();
        }
        true
    }

    fn schedule_isolation_recovery(&self) {
        let expected_epoch = self
            .isolation_recovery_epoch
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        let recovery_epoch = self.isolation_recovery_epoch.clone();
        let connection_states = self.connection_states.clone();
        let signaler = self.signaler.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(
                ISOLATION_RECOVERY_DELAY_MS,
            ))
            .await;
            if recovery_epoch.load(Ordering::SeqCst) != expected_epoch {
                return;
            }
            let isolated = connection_states.read().unwrap().values().all(|state| {
                !matches!(
                    state,
                    ConnectionState::Connected
                        | ConnectionState::Connecting
                        | ConnectionState::Reconnecting
                )
            });
            if !isolated {
                return;
            }
            if let Err(err) = signaler.reset_session().await {
                tracing::warn!("isolated signaling session reset failed: {:?}", err);
            }
        });
    }
}

/// Removes `node`'s entry from `peers` (and, in lock-step, `send_queues` --
/// see `WebRtcTransport::send_queues`'s doc comment) iff `peers` currently
/// still points at the `Peer` behind `expected`. Used by teardown paths that
/// fire asynchronously and without any other serialization against a fresh
/// reconnect for the same `NodeId` (the ICE-state-change and
/// data-channel-close callbacks): a late/stale event belonging to an
/// already-superseded `RTCPeerConnection` must not tear down the *new*,
/// currently-live peer's registration just because it shares the same
/// `NodeId` key. Returns the removed peer only when the identity check
/// passes; otherwise leaves both maps untouched.
async fn remove_peer_if_current(
    peers: &RwLock<HashMap<NodeId, Arc<Peer>>>,
    send_queues: &StdRwLock<HashMap<NodeId, mpsc::Sender<QueuedSend>>>,
    node: &NodeId,
    expected: &Weak<Peer>,
) -> Option<Arc<Peer>> {
    let expected = expected.upgrade()?;
    let mut lock = peers.write().await;
    match lock.get(node) {
        Some(current) if Arc::ptr_eq(current, &expected) => {
            let removed = lock.remove(node);
            drop(lock);
            send_queues.write().unwrap().remove(node);
            removed
        }
        _ => None,
    }
}

impl Peer {
    /// Spawns this peer's ordered send-queue drainer and returns the sender
    /// half, which becomes `Peer::send_tx`.
    ///
    /// Before this existed, `WebRtcTransport::send` called `dc.send()`
    /// directly, and `MistEngine::handle_action_for` (`engine/action.rs`)
    /// invoked it from an independent fire-and-forget `tokio::spawn` per
    /// outbound message. Overlay sequence numbers are stamped synchronously
    /// and in call order (`OverlayRouter::wrap_data`/`next_seq`) before any
    /// of those tasks are spawned, but the spawned tasks then raced each
    /// other for the actual `dc.send()` call -- so the DataChannel write
    /// order (and therefore the order bytes actually left the wire in)
    /// could differ from the seq order, feeding the receiver's
    /// `ReorderBuffer` with self-inflicted reordering purely from this
    /// send-side race, not from the network. Routing every send for a peer
    /// through this single-consumer queue instead makes the DataChannel
    /// write order match the call order that produced the seq numbers,
    /// while sends to *different* peers remain fully concurrent (each
    /// `Peer` has its own queue and drainer task).
    ///
    /// As a side effect this also stops real message loss during a brief
    /// `Open` -> not-`Open` blip (e.g. an ICE restart): a message enqueued
    /// while its target DataChannel isn't `Open` yet waits in the queue
    /// instead of being dropped immediately, and is sent as soon as the
    /// channel (re)opens -- typically well within the peer's disconnect
    /// grace period (`DISCONNECTED_GRACE_MS`), since a peer that never
    /// recovers is reaped by the sweeper, which cancels `cancel_token` and
    /// so tears this drainer down along with the rest of the peer.
    pub(crate) fn spawn_send_queue(
        node: NodeId,
        channels: Arc<RwLock<HashMap<DeliveryMethod, Arc<RTCDataChannel>>>>,
        cancel_token: CancellationToken,
    ) -> mpsc::Sender<QueuedSend> {
        let (tx, mut rx) = mpsc::channel::<QueuedSend>(PEER_SEND_QUEUE_CAPACITY);
        tokio::spawn(async move {
            loop {
                let queued = tokio::select! {
                    _ = cancel_token.cancelled() => break,
                    queued = rx.recv() => match queued {
                        Some(queued) => queued,
                        None => break,
                    },
                };

                // Wait for this message's channel to be `Open`, buffering it
                // (and everything queued behind it) rather than dropping it
                // -- see the doc comment above. Bounded by `cancel_token`:
                // once the peer is torn down (grace period expired, explicit
                // disconnect, ...) this returns `None` and the message below
                // is dropped with the rest of the queue.
                let dc = loop {
                    let candidate = {
                        let channels = channels.read().await;
                        channels.get(&queued.method).cloned()
                    };
                    match candidate {
                        Some(dc) if dc.ready_state() == RTCDataChannelState::Open => {
                            break Some(dc)
                        }
                        Some(_) => {
                            tokio::select! {
                                _ = cancel_token.cancelled() => break None,
                                _ = tokio::time::sleep(Duration::from_millis(SEND_QUEUE_POLL_INTERVAL_MS)) => {}
                            }
                        }
                        None => break None,
                    }
                };

                let Some(dc) = dc else {
                    tracing::warn!(
                        "[SendQueue] dropping queued {:?} message to {} (channel never became \
                         available before the peer was torn down)",
                        queued.method,
                        node
                    );
                    continue;
                };

                if let Err(err) = dc.send(&queued.data).await {
                    tracing::warn!(
                        "[SendQueue] failed to send queued {:?} message to {}: {:?}",
                        queued.method,
                        node,
                        err
                    );
                    continue;
                }
                STATS.add_send_frame(&queued.data);
            }

            // The peer was torn down (cancelled) or `send_tx` was dropped
            // with messages still buffered -- drop them explicitly and warn
            // once with the count, instead of silently discarding when `rx`
            // itself is dropped at the end of this task.
            let mut dropped = 0usize;
            while rx.try_recv().is_ok() {
                dropped += 1;
            }
            if dropped > 0 {
                tracing::warn!(
                    "[SendQueue] dropped {} queued message(s) to {} on peer teardown",
                    dropped,
                    node
                );
            }
        });
        tx
    }

    pub async fn close_all(&self) {
        self.cancel_token.cancel();
        self.detach_peer_handlers();
        let channels = {
            let mut dc_lock = self.channels.write().await;
            std::mem::take(&mut *dc_lock)
        };

        let had_channels = !channels.is_empty();
        for (_, dc) in channels {
            Self::detach_data_channel_handlers(&dc);
            let _ = dc.close().await;
        }
        if had_channels {
            // `RTCDataChannel::close()` above only enqueues an SCTP stream-reset
            // chunk and wakes the association's write loop (see webrtc-sctp's
            // `Stream::shutdown` / `send_reset_request`) -- it returns as soon as
            // the chunk is queued, before it has actually reached the wire. The
            // write loop runs as its own task and needs a real scheduler turn
            // (plus a hop through the blocking pool to marshal the packet) to send
            // it. `pc.close()` below closes the underlying socket before it stops
            // the SCTP association (`Association::close` closes `net_conn` first),
            // so calling it immediately can race ahead of the write loop and
            // silently drop the reset -- leaving the remote peer's `on_close` to
            // wait for the much slower ICE-level disconnect timeout instead of
            // firing promptly. This delay gives the write loop a real chance to
            // flush before we tear down the transport.
            tokio::time::sleep(std::time::Duration::from_millis(CLOSE_FLUSH_DELAY_MS)).await;
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
        self.pc.on_track(Box::new(|_, _, _| Box::pin(async {})));
    }

    /// Adds a local media track (audio/video) to this peer connection, e.g. for
    /// relaying a broadcaster's track to a viewer. Renegotiation (offer/answer)
    /// after calling this is the caller's responsibility.
    pub async fn add_local_track(
        &self,
        track: Arc<dyn TrackLocal + Send + Sync>,
    ) -> crate::error::Result<Arc<RTCRtpSender>> {
        self.pc.add_track(track).await.map_err(Into::into)
    }

    fn detach_data_channel_handlers(dc: &RTCDataChannel) {
        dc.on_open(Box::new(|| Box::pin(async {})));
        dc.on_close(Box::new(|| Box::pin(async {})));
        dc.on_message(Box::new(|_| Box::pin(async {})));
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn setup_handlers(
        self: &Arc<Self>,
        remote_id: NodeId,
        signaler: Arc<dyn Signaler>,
        local_id: NodeId,
        room_id: String,
        event_tx: Option<mpsc::Sender<NetworkEvent>>,
        handles: PeerSharedHandles,
        media_tx: Option<mpsc::UnboundedSender<MediaTrackEvent>>,
    ) -> crate::error::Result<()> {
        self.setup_ice_candidate_handler(remote_id.clone(), signaler, local_id, room_id);
        self.setup_data_channel_handler(remote_id.clone(), event_tx, handles.clone());
        self.setup_track_handler(remote_id.clone(), media_tx);
        self.setup_connection_state_handler(remote_id, handles);
        Ok(())
    }

    fn setup_track_handler(
        self: &Arc<Self>,
        remote_id: NodeId,
        media_tx: Option<mpsc::UnboundedSender<MediaTrackEvent>>,
    ) {
        let Some(media_tx) = media_tx else {
            return;
        };
        let cancel_token = self.cancel_token.clone();
        let pc = self.pc.clone();
        self.pc
            .on_track(Box::new(move |track, receiver, _transceiver| {
                let remote_id = remote_id.clone();
                let media_tx = media_tx.clone();
                let cancel_token = cancel_token.clone();
                let pc = pc.clone();
                Box::pin(async move {
                    if cancel_token.is_cancelled() {
                        return;
                    }
                    let _ = media_tx.send(MediaTrackEvent {
                        remote_id,
                        track,
                        receiver,
                        pc,
                    });
                })
            }));
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
                    let msg = MessageContent::Data(SignalingData {
                        sender_id: local_id,
                        receiver_id: remote_id.clone(),
                        room_id,
                        data,
                        signaling_type: SignalingType::Candidate,
                    });
                    let _ = send_signaling_with_retry(&signaler, &remote_id, &msg, "IceCandidate")
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
                        peer_weak.clone(),
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
        let self_weak = Arc::downgrade(self);
        let connection_states_cb = handles.connection_states.clone();
        let remote_id_cb = remote_id.clone();
        let peers_cb_state_change = Arc::downgrade(&handles.peers);
        let pending_candidates_cb = handles.pending_candidates.clone();
        let attempts_for_state_change = handles.connection_attempt_ids.clone();
        let last_disconnect_at_cb = handles.last_disconnect_at.clone();
        let handshake_permits_cb = handles.handshake_permits.clone();
        self.pc
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                if cancel_token.is_cancelled() {
                    return Box::pin(async {});
                }

                tracing::info!(
                    "[RUST] [{}] peer connection state changed: {:?}",
                    remote_id_cb,
                    s
                );

                match s {
                    RTCPeerConnectionState::Connected => {
                        let state_exists = {
                            let states = handles.connection_states.read().unwrap();
                            states.contains_key(&remote_id_cb)
                        };
                        if state_exists {
                            handles
                                .pc_connected_at
                                .write()
                                .unwrap()
                                .entry(remote_id_cb.clone())
                                .or_insert_with(Instant::now);
                            if handles.recover_connected_from_grace(&remote_id_cb) {
                                tracing::info!(
                                    "[IceRestart] pc re-connected during grace: {} back to Connected",
                                    remote_id_cb
                                );
                            } else {
                                tracing::debug!(
                                    "[WebRTC DC Zombie] pc connected; waiting for ReliableOrdered data channel: {}",
                                    remote_id_cb
                                );
                            }
                        } else {
                            tracing::warn!(
                                "[CS] IGNORE state_change({:?}) for unreserved peer {}",
                                s,
                                remote_id_cb
                            );
                        }
                    }
                    RTCPeerConnectionState::Connecting | RTCPeerConnectionState::New => {
                        if handles.mark_connection_state(&remote_id_cb, ConnectionState::Connecting)
                        {
                            tracing::debug!("[CS] INSERT state_change({:?}): {}", s, remote_id_cb);
                        } else {
                            tracing::warn!(
                                "[CS] IGNORE state_change({:?}) for unreserved peer {}",
                                s,
                                remote_id_cb
                            );
                        }
                    }
                    RTCPeerConnectionState::Disconnected => {
                        // Repair-first ICE restart: `mark_disconnected_grace`
                        // (-> `start_disconnect_grace`) itself fires the
                        // appropriate repair trigger (initiator: rate-limited
                        // ICE restart; non-initiator: a `RestartRequest`
                        // nudge) for a freshly-started grace -- nothing
                        // further to do here beyond the reservation check.
                        let (reserved, _freshly_started) =
                            handles.mark_disconnected_grace(&remote_id_cb);
                        if !reserved {
                            tracing::warn!(
                                "[CS] IGNORE disconnected grace for unreserved peer {}",
                                remote_id_cb
                            );
                        }
                    }
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                        // Everything below -- including the `connection_states`
                        // removal that used to happen synchronously right here --
                        // is deferred into one identity-guarded async block. A
                        // fresh reconnect for `remote_id_cb` can already have
                        // replaced this `RTCPeerConnection`'s bookkeeping with a
                        // brand-new, healthy one by the time a Failed/Closed
                        // event for *this* (stale, already-superseded) pc is
                        // finally delivered -- webrtc-rs state-change callbacks
                        // and a concurrent reconnect race independently, with no
                        // ordering guarantee between them. Tearing down
                        // `connection_states`/`self.peers` unconditionally here
                        // would rip out the *new* connection's state right as it
                        // comes up: this is exactly the "Node not found"
                        // overlay-send race (the new peer's DataChannel opens,
                        // its bootstrap PING/REQUEST_NODE_LIST reply goes through
                        // `wt.send()`, which looks the node up in `self.peers`
                        // and finds it missing because this stale cleanup just
                        // removed it). Gating the whole sequence on `self.peers`
                        // still pointing at *this* Peer (checked in
                        // `remove_peer_if_current`) makes the cleanup a no-op
                        // whenever it would otherwise clobber a superseding
                        // connection.
                        let peers_weak = peers_cb_state_change.clone();
                        let pc_cb = pending_candidates_cb.clone();
                        let remote_id_cb_2 = remote_id_cb.clone();
                        let self_weak_cb = self_weak.clone();
                        let connection_states_cb2 = connection_states_cb.clone();
                        let attempts_cb2 = attempts_for_state_change.clone();
                        let connect_request_attempts_cb =
                            handles.connect_request_attempt_ids.clone();
                        let pc_connected_at_cb = handles.pc_connected_at.clone();
                        // Remote-takeover fix -- see
                        // `WebRtcTransport::established_at`'s doc comment.
                        let established_at_cb = handles.established_at.clone();
                        let handshake_permits_cb2 = handshake_permits_cb.clone();
                        let disconnected_since_cb = handles.disconnected_since.clone();
                        let last_disconnect_at_cb2 = last_disconnect_at_cb.clone();
                        let handles_cb = handles.clone();
                        let state_for_log = s;
                        tokio::spawn(async move {
                            let Some(peers_cb) = peers_weak.upgrade() else {
                                return;
                            };
                            let peer = remove_peer_if_current(
                                &peers_cb,
                                &handles_cb.send_queues,
                                &remote_id_cb_2,
                                &self_weak_cb,
                            )
                            .await;
                            let Some(peer) = peer else {
                                return;
                            };

                            let had_state = {
                                let mut states = connection_states_cb2.write().unwrap();
                                let had_state = states.remove(&remote_id_cb_2).is_some();
                                tracing::debug!(
                                    "[CS] REMOVE state_change({:?}): {} total={}",
                                    state_for_log,
                                    remote_id_cb_2,
                                    states.len()
                                );
                                had_state
                            };
                            {
                                attempts_cb2.write().unwrap().remove(&remote_id_cb_2);
                            }
                            {
                                connect_request_attempts_cb
                                    .write()
                                    .unwrap()
                                    .remove(&remote_id_cb_2);
                            }
                            {
                                pc_connected_at_cb.write().unwrap().remove(&remote_id_cb_2);
                            }
                            {
                                established_at_cb.write().unwrap().remove(&remote_id_cb_2);
                            }
                            {
                                handshake_permits_cb2.write().unwrap().remove(&remote_id_cb_2);
                            }
                            {
                                disconnected_since_cb.write().unwrap().remove(&remote_id_cb_2);
                            }
                            {
                                last_disconnect_at_cb2
                                    .write()
                                    .unwrap()
                                    .insert(remote_id_cb_2.clone(), Instant::now());
                            }
                            {
                                // `[ConnTiming]` instrumentation: confirmed
                                // disconnect via a pc `Failed`/`Closed`
                                // transition -- one of the three sites that
                                // populate `disconnect_observed_at` (see
                                // `WebRtcTransport::disconnect_observed_at`'s
                                // doc comment). Also clear
                                // `connect_started_at` defensively: this path
                                // bypasses `cleanup_session_impl`, and a
                                // Failed/Closed transition can fire before
                                // the DC-open handler ever consumed it (e.g.
                                // a handshake that never got that far).
                                handles_cb
                                    .connect_started_at
                                    .write()
                                    .unwrap()
                                    .remove(&remote_id_cb_2);
                                super::conn_timing::insert_disconnect_observed(
                                    &mut handles_cb.disconnect_observed_at.write().unwrap(),
                                    remote_id_cb_2.clone(),
                                    Instant::now(),
                                );
                            }
                            {
                                pc_cb.write().await.remove(&remote_id_cb_2);
                            }

                            tracing::warn!(
                                "[WebRTC Close] reason=peer_state_{:?} node={}",
                                state_for_log,
                                remote_id_cb_2
                            );
                            peer.close_all().await;
                            crate::mem::record_peer_cleaned();

                            if had_state {
                                // `[ConnTiming]` instrumentation: this path
                                // bypasses `cleanup_session_impl` entirely
                                // (see the comment there), so it emits its
                                // own `kind=disconnect` line here, gated on
                                // the same `had_state` check that gates
                                // `on_disconnected_internal` below. Mutual
                                // exclusion with `cleanup_session_impl`'s
                                // emission is structural: both ultimately
                                // depend on `remove_peer_if_current`'s
                                // identity-guarded removal, which succeeds at
                                // most once per `Peer`.
                                let reason = format!("peer_state_{:?}", state_for_log);
                                super::conn_timing::log_disconnect(&remote_id_cb_2, &reason);
                                crate::events::on_disconnected_internal(
                                    handles_cb.room_id.clone(),
                                    remote_id_cb_2,
                                );
                                handles_cb.schedule_isolation_recovery();
                            }
                        });
                    }
                    _ => {}
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
        peer_weak: Weak<Peer>,
    ) {
        let method = match dc.label() {
            "reliable" => DeliveryMethod::ReliableOrdered,
            "unreliable-ordered" => DeliveryMethod::UnreliableOrdered,
            "unreliable" => DeliveryMethod::Unreliable,
            _ => DeliveryMethod::ReliableOrdered,
        };
        let room_id = handles.room_id.clone();
        Self::setup_dc_open_handler(
            &dc,
            remote_id.clone(),
            method,
            handles.connection_states.clone(),
            handles.disconnected_since.clone(),
            handles.pc_connected_at.clone(),
            handles.established_at.clone(),
            handles.handshake_permits.clone(),
            handles.connect_started_at.clone(),
            handles.disconnect_observed_at.clone(),
            cancel_token.clone(),
            room_id,
        );
        Self::setup_dc_close_handler(
            &dc,
            remote_id.clone(),
            handles,
            cancel_token.clone(),
            peer_weak,
        );
        Self::setup_dc_message_handler(&dc, remote_id, event_tx, cancel_token);
    }

    #[allow(clippy::too_many_arguments)]
    fn setup_dc_open_handler(
        dc: &Arc<RTCDataChannel>,
        remote_id: NodeId,
        method: DeliveryMethod,
        states: Arc<StdRwLock<HashMap<NodeId, ConnectionState>>>,
        disconnected_since: Arc<StdRwLock<HashMap<NodeId, DisconnectGrace>>>,
        pc_connected_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
        established_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
        handshake_permits: Arc<StdRwLock<HashMap<NodeId, OwnedSemaphorePermit>>>,
        connect_started_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
        disconnect_observed_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
        cancel_token: CancellationToken,
        room_id: String,
    ) {
        dc.on_open(Box::new(move || {
            let remote_id = remote_id.clone();
            let states = states.clone();
            let disconnected_since = disconnected_since.clone();
            // Named `_map` to avoid shadowing the `established_at: Instant`
            // local bound just below, inside the `!already_connected` arm.
            let established_at_map = established_at.clone();
            let cancel = cancel_token.clone();
            let room_id = room_id.clone();
            Box::pin(async move {
                if cancel.is_cancelled() {
                    return;
                }
                if method != DeliveryMethod::ReliableOrdered {
                    tracing::debug!(
                        "[Conn] DataChannel opened: {} method={:?}",
                        remote_id,
                        method
                    );
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
                let already_connected = matches!(lock.get(&remote_id), Some(ConnectionState::Connected));
                lock.insert(remote_id.clone(), ConnectionState::Connected);
                let total = lock.len();
                drop(lock);
                disconnected_since.write().unwrap().remove(&remote_id);
                pc_connected_at.write().unwrap().remove(&remote_id);
                handshake_permits.write().unwrap().remove(&remote_id);
                tracing::warn!(
                    "[WebRTC DC Zombie] recovered: ReliableOrdered data channel opened for {}",
                    remote_id
                );
                tracing::info!(
                    "[Conn] ReliableOrdered DataChannel opened: {} -> Connected (total_connected={})",
                    remote_id.0,
                    total
                );
                if !already_connected {
                    // `[ConnTiming]` instrumentation: this is the
                    // establishment point (ReliableOrdered data channel
                    // open, transitioning to `Connected`). `connect_started_at`
                    // is consumed (removed) here; if it's missing (shouldn't
                    // normally happen -- every attempt reserves one) skip the
                    // log rather than emit a line with a made-up duration.
                    let established_at = Instant::now();
                    // Remote-takeover fix: record this establishment moment
                    // for `takeover_allowed`'s recent-connect guard -- see
                    // `WebRtcTransport::established_at`'s doc comment. Set
                    // unconditionally here (not gated on any later step
                    // succeeding), since data-channel establishment is itself
                    // the thing the guard cares about.
                    established_at_map
                        .write()
                        .unwrap()
                        .insert(remote_id.clone(), established_at);
                    let started_at = connect_started_at.write().unwrap().remove(&remote_id);
                    if let Some(started_at) = started_at {
                        let attempt_ms =
                            established_at.saturating_duration_since(started_at).as_millis() as u64;
                        let observed_disconnect_at =
                            disconnect_observed_at.write().unwrap().remove(&remote_id);
                        if let Some(observed_disconnect_at) = observed_disconnect_at {
                            let downtime_ms = established_at
                                .saturating_duration_since(observed_disconnect_at)
                                .as_millis() as u64;
                            super::conn_timing::log_reconnect(
                                &remote_id,
                                attempt_ms,
                                downtime_ms,
                                total,
                            );
                        } else {
                            super::conn_timing::log_connect(&remote_id, attempt_ms, total);
                        }
                    }
                    crate::events::on_connected_internal(room_id, remote_id);
                }
            })
        }));
    }

    fn setup_dc_close_handler(
        dc: &Arc<RTCDataChannel>,
        remote_id: NodeId,
        handles: PeerSharedHandles,
        cancel_token: CancellationToken,
        peer_weak: Weak<Peer>,
    ) {
        let dc_for_close = dc.clone();
        dc.on_close(Box::new(move || {
            let remote_id = remote_id.clone();
            let states = handles.connection_states.clone();
            let peers = handles.peers.clone();
            let send_queues = handles.send_queues.clone();
            let pending = handles.pending_candidates.clone();
            let attempts = handles.connection_attempt_ids.clone();
            let connect_request_attempts = handles.connect_request_attempt_ids.clone();
            let pc_connected_at = handles.pc_connected_at.clone();
            let established_at = handles.established_at.clone();
            let last_disconnect = handles.last_disconnect_at.clone();
            let disconnected_since = handles.disconnected_since.clone();
            let handshake_permits = handles.handshake_permits.clone();
            let connect_started_at = handles.connect_started_at.clone();
            let disconnect_observed_at = handles.disconnect_observed_at.clone();
            let room_id = handles.room_id.clone();
            let dc = dc_for_close.clone();
            let cancel = cancel_token.clone();
            let peer_weak = peer_weak.clone();
            // If this is an unintentional disconnect (not a programmatic close),
            // schedule isolation recovery here. The connection-state handler would
            // normally do this, but it returns early when cancel_token is already
            // set — which happens because close_all() in the async block below
            // cancels the token before the state-change event fires.
            if !cancel_token.is_cancelled() {
                handles.schedule_isolation_recovery();
            }
            Box::pin(async move {
                if cancel.is_cancelled() {
                    return;
                }
                tracing::warn!("[RUST] [{}] data channel closed: {}", remote_id, dc.label());
                // Only tear down `remote_id`'s bookkeeping if `peers` still maps
                // it to *this* Peer instance. A reconnect can already have
                // replaced this entry with a brand-new (already-open, healthy)
                // Peer by the time this close callback -- for the old,
                // now-superseded RTCDataChannel -- finally runs; blindly
                // removing by NodeId here would rip out the live connection's
                // state instead of the stale one's (see the "Node not found"
                // overlay-send race this guards against).
                let peer =
                    remove_peer_if_current(&peers, &send_queues, &remote_id, &peer_weak).await;
                let Some(peer) = peer else { return };
                {
                    let mut lock = attempts.write().unwrap();
                    lock.remove(&remote_id);
                }
                {
                    let mut lock = connect_request_attempts.write().unwrap();
                    lock.remove(&remote_id);
                }
                {
                    let mut lock = pc_connected_at.write().unwrap();
                    lock.remove(&remote_id);
                }
                {
                    // Remote-takeover fix -- see
                    // `WebRtcTransport::established_at`'s doc comment.
                    let mut lock = established_at.write().unwrap();
                    lock.remove(&remote_id);
                }
                {
                    let mut lock = handshake_permits.write().unwrap();
                    lock.remove(&remote_id);
                }
                {
                    let mut lock = last_disconnect.write().unwrap();
                    lock.insert(remote_id.clone(), Instant::now());
                }
                {
                    // `[ConnTiming]` instrumentation: confirmed disconnect
                    // via the ReliableOrdered data channel closing -- one of
                    // the three sites that populate `disconnect_observed_at`
                    // (see `WebRtcTransport::disconnect_observed_at`'s doc
                    // comment). Also clear `connect_started_at` defensively:
                    // this path bypasses `cleanup_session_impl`.
                    connect_started_at.write().unwrap().remove(&remote_id);
                    super::conn_timing::insert_disconnect_observed(
                        &mut disconnect_observed_at.write().unwrap(),
                        remote_id.clone(),
                        Instant::now(),
                    );
                }
                {
                    let mut lock = disconnected_since.write().unwrap();
                    lock.remove(&remote_id);
                }
                {
                    let mut lock = states.write().unwrap();
                    lock.remove(&remote_id);
                }
                {
                    let mut lock = pending.write().await;
                    lock.remove(&remote_id);
                }
                tracing::warn!(
                    "[WebRTC Close] reason=data_channel_close node={} label={}",
                    remote_id,
                    dc.label()
                );
                peer.close_all().await;
                crate::mem::record_peer_cleaned();
                crate::events::on_disconnected_internal(room_id, remote_id);
            })
        }));
    }

    async fn forward_dc_message(
        event_tx: Option<mpsc::Sender<NetworkEvent>>,
        remote_id: NodeId,
        data: Bytes,
    ) {
        let Some(tx) = event_tx else {
            tracing::debug!(
                "dropping received data channel event for {remote_id}: no event handler"
            );
            STATS.add_dropped_receive_event();
            return;
        };

        if tx
            .send(NetworkEvent {
                from: remote_id.clone(),
                data,
            })
            .await
            .is_err()
        {
            tracing::debug!(
                "dropping received data channel event for {remote_id}: forwarder closed"
            );
            STATS.add_dropped_receive_event();
        }
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
                    Self::forward_dc_message(tx, remote_id, msg.data).await;
                })
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the "Node not found" overlay-send race: a peer
    /// connection that fails/closes *after* a fresh reconnect has already
    /// replaced its `self.peers` entry for the same `NodeId` must not tear
    /// down the new, live peer's registration. `remove_peer_if_current` is
    /// what `setup_connection_state_handler`'s `Failed`/`Closed` arm and
    /// `setup_dc_close_handler` both now gate their cleanup on -- this
    /// exercises the guard directly against two real `Peer`s for the same
    /// node, which is exactly the shape of the race (an old, stale `Peer`
    /// instance whose teardown callback fires late, after a newer `Peer`
    /// already owns the `NodeId` key).
    #[tokio::test]
    async fn remove_peer_if_current_ignores_a_stale_peer_already_superseded_by_a_reconnect() {
        let t = crate::transports::webrtc::tests::make_transport();
        let node = NodeId("stale-vs-fresh".to_string());

        let old_peer = t
            .create_pc(node.clone())
            .await
            .expect("old peer should be created");
        let new_peer = t
            .create_pc(node.clone())
            .await
            .expect("new peer should be created");
        assert!(
            !Arc::ptr_eq(&old_peer, &new_peer),
            "test setup should produce two distinct Peer instances"
        );

        // A fresh reconnect has already replaced the map entry with the new,
        // healthy peer -- mirroring `replace_peer_and_close_old`/
        // `handle_offer`'s `peers.insert` (and its matching `send_queues`
        // insert -- see `WebRtcTransport::send_queues`'s doc comment).
        t.peers.write().await.insert(node.clone(), new_peer.clone());
        t.send_queues
            .write()
            .unwrap()
            .insert(node.clone(), new_peer.send_tx.clone());

        // The *old* peer's Failed/Closed (or dc-close) callback finally runs,
        // asking to remove/close `node`'s entry. It must be a no-op: the map
        // no longer belongs to it.
        let old_weak = Arc::downgrade(&old_peer);
        let removed = remove_peer_if_current(&t.peers, &t.send_queues, &node, &old_weak).await;
        assert!(
            removed.is_none(),
            "a stale peer's cleanup must not remove a newer peer registered under the same NodeId"
        );
        let current = t.peers.read().await.get(&node).cloned();
        assert!(
            matches!(current, Some(p) if Arc::ptr_eq(&p, &new_peer)),
            "the new peer's registration must survive the stale peer's cleanup attempt"
        );
        assert!(
            t.send_queues.read().unwrap().contains_key(&node),
            "a stale peer's cleanup must not remove the new peer's send_queues entry either"
        );

        // The *current* peer's own teardown must still work normally.
        let new_weak = Arc::downgrade(&new_peer);
        let removed = remove_peer_if_current(&t.peers, &t.send_queues, &node, &new_weak).await;
        assert!(
            matches!(removed, Some(p) if Arc::ptr_eq(&p, &new_peer)),
            "the current peer must be removable by its own cleanup"
        );
        assert!(t.peers.read().await.get(&node).is_none());
        assert!(
            !t.send_queues.read().unwrap().contains_key(&node),
            "removing the current peer must also remove its send_queues entry"
        );
    }

    #[tokio::test]
    async fn forward_dc_message_waits_when_forwarder_channel_is_full() {
        let (tx, mut rx) = mpsc::channel::<NetworkEvent>(1);
        let peer = NodeId("peer-a".to_string());

        Peer::forward_dc_message(Some(tx.clone()), peer.clone(), Bytes::from_static(b"first"))
            .await;

        let second = tokio::spawn(Peer::forward_dc_message(
            Some(tx),
            peer.clone(),
            Bytes::from_static(b"second"),
        ));

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!second.is_finished());

        let first = rx.recv().await.unwrap();
        assert_eq!(first.from, peer);
        assert_eq!(&first.data[..], b"first");

        second.await.unwrap();
        let second = rx.recv().await.unwrap();
        assert_eq!(second.from, NodeId("peer-a".to_string()));
        assert_eq!(&second.data[..], b"second");
    }

    /// A `Signaler` that fails its first `fail_count` calls to
    /// `send_signaling` (recording every attempt), then succeeds on every
    /// call after that. Used to drive [`send_signaling_with_retry`] through
    /// exactly the transient-failure-then-recovery shape a real signaling
    /// layer would produce (e.g. `RoutedSignaler` returning `RouteNotFound`
    /// for a route that hasn't caught up yet, then succeeding a moment
    /// later).
    struct FlakySignaler {
        fail_count: usize,
        attempts: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Signaler for FlakySignaler {
        async fn send_signaling(
            &self,
            _to: &NodeId,
            _msg: MessageContent,
        ) -> mistlib_core::error::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt <= self.fail_count {
                Err(mistlib_core::error::MistError::Internal(format!(
                    "simulated transient failure (attempt {})",
                    attempt
                )))
            } else {
                Ok(())
            }
        }

        async fn close(&self) -> mistlib_core::error::Result<()> {
            Ok(())
        }
    }

    fn test_signaling_msg() -> MessageContent {
        MessageContent::Data(SignalingData {
            sender_id: NodeId("local".to_string()),
            receiver_id: NodeId("remote".to_string()),
            room_id: "room".to_string(),
            data: "payload".to_string(),
            signaling_type: SignalingType::Candidate,
        })
    }

    /// Regression test for the ICE-candidate-send fix: a signaling send that
    /// fails on its first attempt(s) but recovers within
    /// `SIGNALING_SEND_RETRY_ATTEMPTS` must still succeed overall, instead of
    /// the single transient failure being the end of the story (the old
    /// `let _ = signaler.send_signaling(...).await;` behavior).
    #[tokio::test]
    async fn send_signaling_with_retry_recovers_from_transient_failures() {
        let signaler: Arc<dyn Signaler> = Arc::new(FlakySignaler {
            fail_count: SIGNALING_SEND_RETRY_ATTEMPTS as usize - 1,
            attempts: std::sync::atomic::AtomicUsize::new(0),
        });
        let to = NodeId("remote".to_string());

        let result = send_signaling_with_retry(&signaler, &to, &test_signaling_msg(), "test").await;

        assert!(
            result.is_ok(),
            "a send that recovers within the attempt budget must ultimately succeed"
        );
    }

    /// Counterpart: once every attempt is exhausted, the retry must still
    /// report failure (not silently swallow it forever) so the final `warn`
    /// log fires and callers that check the `Result` (like
    /// `try_ice_restart_once`) can react.
    #[tokio::test]
    async fn send_signaling_with_retry_reports_failure_after_exhausting_all_attempts() {
        let signaler: Arc<dyn Signaler> = Arc::new(FlakySignaler {
            fail_count: SIGNALING_SEND_RETRY_ATTEMPTS as usize + 5,
            attempts: std::sync::atomic::AtomicUsize::new(0),
        });
        let to = NodeId("remote".to_string());

        let result = send_signaling_with_retry(&signaler, &to, &test_signaling_msg(), "test").await;

        assert!(
            result.is_err(),
            "a send that never recovers within the attempt budget must report failure"
        );
    }

    /// Exactly `SIGNALING_SEND_RETRY_ATTEMPTS` attempts must be made -- not
    /// more (that would be an unbounded/too-long retry) and not fewer (that
    /// would silently drop the bounded-retry contract this fix adds).
    #[tokio::test]
    async fn send_signaling_with_retry_makes_exactly_the_configured_number_of_attempts() {
        let attempts_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct CountingAlwaysFailSignaler(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl Signaler for CountingAlwaysFailSignaler {
            async fn send_signaling(
                &self,
                _to: &NodeId,
                _msg: MessageContent,
            ) -> mistlib_core::error::Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(mistlib_core::error::MistError::Internal(
                    "always fails".to_string(),
                ))
            }
            async fn close(&self) -> mistlib_core::error::Result<()> {
                Ok(())
            }
        }
        let signaler: Arc<dyn Signaler> =
            Arc::new(CountingAlwaysFailSignaler(attempts_counter.clone()));
        let to = NodeId("remote".to_string());

        let result = send_signaling_with_retry(&signaler, &to, &test_signaling_msg(), "test").await;

        assert!(result.is_err());
        assert_eq!(
            attempts_counter.load(Ordering::SeqCst),
            SIGNALING_SEND_RETRY_ATTEMPTS as usize,
            "must attempt exactly SIGNALING_SEND_RETRY_ATTEMPTS times, no more, no less"
        );
    }

    /// `try_ice_restart` must not retry when there is no live peer at all --
    /// nothing about retrying would ever bring a removed peer back, so it
    /// should return promptly on the very first attempt instead of running
    /// out `ICE_RESTART_RETRY_ATTEMPTS` worth of backoff for nothing.
    #[tokio::test]
    async fn try_ice_restart_does_not_retry_when_there_is_no_live_peer() {
        let handles = crate::transports::webrtc::tests::make_transport().peer_handles();
        let node = NodeId("no-such-peer".to_string());

        let start = std::time::Instant::now();
        handles.try_ice_restart(&node).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(ICE_RESTART_RETRY_INITIAL_BACKOFF_MS),
            "a missing peer must short-circuit immediately, not burn through the retry backoff \
             (elapsed={:?})",
            elapsed
        );
    }

    /// Regression test for the `try_ice_restart` retry fix: a signaling
    /// state that isn't `Stable` when the first attempt runs (some other
    /// negotiation already in flight) used to mean giving up immediately.
    /// Now it retries across `ICE_RESTART_RETRY_ATTEMPTS` attempts with a
    /// backoff between them -- observable here as the call taking measurably
    /// longer than a single immediate check would, since the signaling state
    /// in this test never becomes `Stable` again.
    #[tokio::test]
    async fn try_ice_restart_retries_across_backoff_when_signaling_state_stays_unstable() {
        let t = crate::transports::webrtc::tests::make_transport();
        let node = NodeId("perpetually-unstable-peer".to_string());
        let peer = t
            .create_pc(node.clone())
            .await
            .expect("peer connection should be created for this test");

        // Wedge signaling state away from `Stable` -- mirrors what an
        // in-flight renegotiation looks like from `try_ice_restart`'s point
        // of view (`send_offer`'s own precondition check treats this the
        // same way).
        let offer = peer
            .pc
            .create_offer(None)
            .await
            .expect("create_offer should succeed on a fresh pc");
        peer.pc
            .set_local_description(offer)
            .await
            .expect("set_local_description should succeed on a fresh pc");
        assert_ne!(peer.pc.signaling_state(), RTCSignalingState::Stable);

        t.peers.write().await.insert(node.clone(), peer);

        let start = std::time::Instant::now();
        t.peer_handles().try_ice_restart(&node).await;
        let elapsed = start.elapsed();

        // With ICE_RESTART_RETRY_ATTEMPTS attempts there are
        // (ICE_RESTART_RETRY_ATTEMPTS - 1) backoff sleeps, growing
        // exponentially rather than a fixed interval -- sum the real
        // schedule instead of assuming `count * constant`.
        let expected_min =
            std::time::Duration::from_millis(crate::transports::webrtc::backoff::total_backoff_ms(
                ICE_RESTART_RETRY_ATTEMPTS - 1,
                ICE_RESTART_RETRY_INITIAL_BACKOFF_MS,
                ICE_RESTART_RETRY_BACKOFF_MULTIPLIER,
                ICE_RESTART_RETRY_MAX_BACKOFF_MS,
            ));
        assert!(
            elapsed >= expected_min,
            "expected try_ice_restart to retry across every attempt's backoff (elapsed={:?}, expected_min={:?})",
            elapsed,
            expected_min
        );
    }

    /// Records every message handed to `send_signaling`, tagged with its
    /// `SignalingType` -- used by the `spawn_offer_resend` tests below to
    /// count how many `Offer`s were (re)sent.
    struct RecordingSignaler(std::sync::Mutex<Vec<MessageContent>>);

    #[async_trait::async_trait]
    impl Signaler for RecordingSignaler {
        async fn send_signaling(
            &self,
            _to: &NodeId,
            msg: MessageContent,
        ) -> mistlib_core::error::Result<()> {
            self.0.lock().unwrap().push(msg);
            Ok(())
        }

        async fn close(&self) -> mistlib_core::error::Result<()> {
            Ok(())
        }
    }

    fn count_offers(recorder: &RecordingSignaler) -> usize {
        recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|msg| {
                matches!(msg, MessageContent::Data(d) if d.signaling_type == SignalingType::Offer)
            })
            .count()
    }

    /// Sets `node` up as a fresh, unanswered outbound offer: a real `Peer`
    /// registered in `t.peers`, `connection_states` reserved as `Connecting`
    /// (`send_offer`'s precondition), and the initial offer already sent via
    /// `t.signaler`. Returns `(peer, attempt_id)` -- everything
    /// `spawn_offer_resend` needs.
    async fn setup_unanswered_offer(
        t: &crate::transports::webrtc::WebRtcTransport,
        node: &NodeId,
    ) -> (Arc<Peer>, u32) {
        let peer = t
            .create_pc(node.clone())
            .await
            .expect("peer should be created");
        // A real m= section (an application/data-channel one here) so a
        // throwaway remote could answer this offer meaningfully -- some of
        // these tests do exactly that.
        peer.pc
            .create_data_channel("reliable", None)
            .await
            .expect("data channel should be created");
        t.peers.write().await.insert(node.clone(), peer.clone());
        t.connection_states
            .write()
            .unwrap()
            .insert(node.clone(), ConnectionState::Connecting);

        t.send_offer(node, &peer)
            .await
            .expect("initial offer send should succeed");
        let attempt_id = t.reserve_connection_attempt(node);
        (peer, attempt_id)
    }

    /// Offer resend, initiator side (see
    /// `WebRtcTransport::spawn_offer_resend`'s doc comment): while the peer
    /// stays in `HaveLocalOffer` (no answer ever arrives) and neither the
    /// peer nor the attempt is superseded, every scheduled resend in
    /// `super::OFFER_RESEND_SCHEDULE_MS` must actually fire -- this is the
    /// bounded-retransmission mechanism the whole fix exists to add.
    #[tokio::test]
    async fn spawn_offer_resend_retransmits_the_current_offer_while_unanswered() {
        let recorder = Arc::new(RecordingSignaler(std::sync::Mutex::new(Vec::new())));
        let t = crate::transports::webrtc::WebRtcTransport::new(
            recorder.clone() as Arc<dyn Signaler>,
            NodeId("local".to_string()),
        );
        let node = NodeId("remote".to_string());
        let (peer, attempt_id) = setup_unanswered_offer(&t, &node).await;

        t.spawn_offer_resend(node.clone(), attempt_id, peer.clone());

        // Both schedule entries (test values 30ms/60ms) plus their jitter
        // (test bound: 5ms) must have had a chance to fire.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert_eq!(
            count_offers(&recorder),
            1 + crate::transports::webrtc::OFFER_RESEND_MAX as usize,
            "the initial send plus every scheduled resend must have fired while the offer \
             stayed unanswered"
        );
        assert_eq!(
            peer.pc.signaling_state(),
            RTCSignalingState::HaveLocalOffer,
            "sanity: nothing in this test ever answers the offer"
        );
    }

    /// Counterpart: once the offer IS answered (signaling_state leaves
    /// `HaveLocalOffer` for `Stable`), no further resend may fire -- a
    /// resend past that point would be a stale, pointless retransmission
    /// (the negotiation this offer started has already completed).
    #[tokio::test]
    async fn spawn_offer_resend_stops_once_the_offer_is_answered() {
        use webrtc::peer_connection::configuration::RTCConfiguration;

        let recorder = Arc::new(RecordingSignaler(std::sync::Mutex::new(Vec::new())));
        let t = crate::transports::webrtc::WebRtcTransport::new(
            recorder.clone() as Arc<dyn Signaler>,
            NodeId("local".to_string()),
        );
        let node = NodeId("remote".to_string());
        let (peer, attempt_id) = setup_unanswered_offer(&t, &node).await;

        // Answer the offer on a throwaway peer connection and apply that
        // answer to our real peer directly -- moves `peer.pc` to `Stable`
        // exactly the way a real `handle_answer` would, without needing a
        // second full transport/real network (mirrors
        // `tests/takeover.rs`'s `build_offer` helper's use of a throwaway
        // `RTCPeerConnection` for offer-side SDP; here it's the answer
        // side).
        let offer_sdp = peer
            .pc
            .local_description()
            .await
            .expect("offer should be set as local description")
            .sdp;
        let fake_remote = t
            .api
            .new_peer_connection(RTCConfiguration::default())
            .await
            .expect("throwaway peer connection should build");
        fake_remote
            .set_remote_description(
                crate::transports::webrtc::signaling::parse_offer_payload(&offer_sdp)
                    .expect("offer SDP should parse"),
            )
            .await
            .expect("throwaway pc should accept our offer");
        let answer = fake_remote
            .create_answer(None)
            .await
            .expect("throwaway pc should answer");
        fake_remote
            .set_local_description(answer.clone())
            .await
            .expect("throwaway pc should set its own answer");
        peer.pc
            .set_remote_description(answer)
            .await
            .expect("our peer should accept the answer");
        assert_eq!(peer.pc.signaling_state(), RTCSignalingState::Stable);

        t.spawn_offer_resend(node.clone(), attempt_id, peer.clone());
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert_eq!(
            count_offers(&recorder),
            1,
            "once the offer is answered (signaling_state left HaveLocalOffer) no resend must \
             fire -- only the original send may be recorded"
        );
    }

    /// Counterpart: once `attempt_id` is superseded (a fresh `connect_inner`/
    /// takeover reserved a brand-new attempt id for the same node) before the
    /// first scheduled resend fires, the resend task for the old attempt must
    /// stop silently rather than resending on behalf of an attempt that no
    /// longer owns this node.
    #[tokio::test]
    async fn spawn_offer_resend_stops_once_the_attempt_is_superseded() {
        let recorder = Arc::new(RecordingSignaler(std::sync::Mutex::new(Vec::new())));
        let t = crate::transports::webrtc::WebRtcTransport::new(
            recorder.clone() as Arc<dyn Signaler>,
            NodeId("local".to_string()),
        );
        let node = NodeId("remote".to_string());
        let (peer, attempt_id) = setup_unanswered_offer(&t, &node).await;

        t.spawn_offer_resend(node.clone(), attempt_id, peer.clone());
        // Supersede immediately, before the first scheduled resend (test
        // value: 30ms + <=5ms jitter) has any chance to fire.
        let _new_attempt_id = t.reserve_connection_attempt(&node);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert_eq!(
            count_offers(&recorder),
            1,
            "a resend task for a superseded attempt must never resend, leaving only the \
             original send"
        );
    }
}
