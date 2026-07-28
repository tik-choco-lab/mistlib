use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::signaling_state::RTCSignalingState;

use mistlib_core::signaling::{MessageContent, SignalingData, SignalingType};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};

#[cfg(test)]
const SWEEPER_INTERVAL_MS: u64 = 10;
#[cfg(not(test))]
const SWEEPER_INTERVAL_MS: u64 = 2000;

/// Sweeper livelock fix: how long the no-peer-registered branch waits before
/// treating a bare `Connecting`/`Reconnecting` reservation (a
/// `connection_states` entry with nothing yet in `self.peers`) as abandoned
/// rather than merely queued on `acquire_handshake_permit` -- see
/// `reservation_reap_allowed` and `WebRtcTransport::connecting_reserved_at`'s
/// doc comment for the livelock this closes. In production this must
/// outlive a full connect attempt (`super::CONNECTION_TIMEOUT_MS`) plus one
/// more sweep tick (`SWEEPER_INTERVAL_MS`), so a reservation still
/// legitimately queued for a handshake permit is never reaped mid-wait.
/// Tracked as its own constant rather than computed from
/// `super::CONNECTION_TIMEOUT_MS` at every reference (which this
/// deliberately leaves untouched, including its lack of a `#[cfg(test)]`
/// shrink -- see that constant's own doc comment and this fix's "do not
/// touch... watchdog timeout" non-goal) so its own `#[cfg(test)]` value can
/// be small like every other sweeper-timing constant here, independent of
/// the connect-timeout watchdog's timing. No env override: like
/// `REMOTE_TAKEOVER_RECENT_CONNECT_MS` and friends, this is a correctness
/// guard, not an eval-harness tuning knob.
#[cfg(test)]
pub(crate) const CONNECTING_RESERVATION_REAP_GATE_MS: u64 = 60;
#[cfg(not(test))]
pub(crate) const CONNECTING_RESERVATION_REAP_GATE_MS: u64 =
    super::CONNECTION_TIMEOUT_MS + SWEEPER_INTERVAL_MS;

/// Pure predicate backing the sweeper's no-peer-registered branch: whether a
/// bare reservation should be reaped right now. `reserved_at` is `None` when
/// no reservation timestamp was ever recorded for this node -- every current
/// reservation path (`Transport::connect`, `signaling::handle_offer`) always
/// records one, so a missing entry here is itself an anomaly (e.g. state
/// from before this fix, or a bug elsewhere) rather than a legitimately
/// queued dial, and is treated as immediately reapable, matching this
/// branch's original (always-reap) behavior for that case. Boundary is
/// inclusive (`elapsed == gate_ms` reaps), matching `takeover_allowed`'s own
/// strict-`<`-blocks convention. Pure so the boundary is exhaustively
/// unit-testable without a live handshake -- see `webrtc/tests/sweeper.rs`.
pub(crate) fn reservation_reap_allowed(reserved_at: Option<Instant>, gate_ms: u64) -> bool {
    reserved_at.is_none_or(|at| at.elapsed() >= Duration::from_millis(gate_ms))
}

use super::{DisconnectGrace, GraceOrigin, Peer, WebRtcTransport, DATA_CHANNEL_OPEN_TIMEOUT_MS};

/// Outcome of evaluating a peer's disconnect grace against its actual health.
/// Pure (no lock/`RTCPeerConnection` access) so the `LivenessSuspect`
/// false-positive suppression below is exhaustively unit-testable without
/// mocking a real `RTCPeerConnection` -- see `GraceOrigin`'s doc for why the
/// distinction exists: a liveness-suspect grace can start against a peer
/// whose SCTP association (and every data channel except the best-effort
/// ping one) never actually had a problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraceExpiryDecision {
    /// No grace is running, or it hasn't reached the grace duration yet.
    Wait,
    /// Grace expired, but the peer looks genuinely healthy (only possible for
    /// a `LivenessSuspect`-origin grace): restore `Connected` instead of
    /// tearing the session down.
    RecoverFalsePositive,
    /// Grace expired and the peer is still unhealthy, or this is an
    /// `Ice`-origin grace (never second-guessed against the live pc state) --
    /// reap as before.
    Reap,
}

/// Decides what the sweeper should do about `node`'s disconnect grace, if
/// any. `grace` is a snapshot of `disconnected_since[node]`; `pc_state` and
/// `has_required_data_channel` are the sweeper's own live reads of the actual
/// `RTCPeerConnection`/data-channel state for the same node.
///
/// Only a `LivenessSuspect`-origin grace is ever second-guessed: an
/// `Ice`-origin grace means the peer connection itself already reported
/// `Disconnected`, so there is nothing to re-validate. A `LivenessSuspect`
/// grace, by contrast, is started purely on a missed-PONG heuristic
/// (`OverlayAction::SuspectDisconnected`, mistlib-core's `stats::ping`) that
/// runs over the best-effort `Unreliable` channel (`max_retransmits: Some(0)`)
/// and never itself inspects the real peer connection -- packet loss on that
/// one channel alone is not evidence that the `ReliableOrdered` channel
/// actually carrying application data (e.g. a tunneled SSH session) is
/// unhealthy. Reaping on that signal alone tears down a perfectly good
/// connection; this lets the sweeper re-check reality before doing so.
pub(crate) fn decide_grace_expiry(
    grace: Option<DisconnectGrace>,
    pc_state: RTCPeerConnectionState,
    has_required_data_channel: bool,
    grace_ms: u64,
) -> GraceExpiryDecision {
    let Some(grace) = grace else {
        return GraceExpiryDecision::Wait;
    };
    if grace.started_at.elapsed() < Duration::from_millis(grace_ms) {
        return GraceExpiryDecision::Wait;
    }
    if grace.origin == GraceOrigin::LivenessSuspect
        && pc_state == RTCPeerConnectionState::Connected
        && has_required_data_channel
    {
        return GraceExpiryDecision::RecoverFalsePositive;
    }
    GraceExpiryDecision::Reap
}

impl WebRtcTransport {
    pub(crate) fn data_channel_open_timeout() -> Duration {
        std::env::var("MIST_WEBRTC_DC_OPEN_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(DATA_CHANNEL_OPEN_TIMEOUT_MS))
    }

    pub(crate) async fn has_required_data_channel(peer: &Peer) -> bool {
        let channels = peer.channels.read().await;
        channels
            .get(&DeliveryMethod::ReliableOrdered)
            .is_some_and(|dc| dc.ready_state() == RTCDataChannelState::Open)
    }

    pub(crate) fn spawn_connection_watchdog(&self, node: NodeId, attempt_id: u32) {
        let handles = self.peer_handles();
        // Captured before the spawn so a per-instance
        // `MIST_WEBRTC_CONNECTION_TIMEOUT_MS` override (read once at
        // construction, see `WebRtcTransport::connection_timeout_ms`)
        // applies here without needing `&self` inside the spawned task.
        let connection_timeout_ms = self.connection_timeout_ms;

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(connection_timeout_ms)).await;

            let is_current_attempt = {
                let lock = handles.connection_attempt_ids.read().unwrap();
                matches!(lock.get(&node), Some(id) if *id == attempt_id)
            };

            if !is_current_attempt {
                return;
            }

            let still_connecting = {
                let lock = handles.connection_states.read().unwrap();
                matches!(lock.get(&node), Some(ConnectionState::Connecting))
            };

            let peer_opt = {
                let lock = handles.peers.read().await;
                lock.get(&node).cloned()
            };
            let has_required_data_channel = if let Some(peer) = &peer_opt {
                let channels = peer.channels.read().await;
                channels
                    .get(&DeliveryMethod::ReliableOrdered)
                    .is_some_and(|dc| dc.ready_state() == RTCDataChannelState::Open)
            } else {
                false
            };

            if still_connecting && !has_required_data_channel {
                // `[ConnTiming]` instrumentation: this is the one-shot
                // connection watchdog force-cleaning a still-`Connecting`
                // session -- report the elapsed time as a timeout and remove
                // the attempt-start entry so it can't leak (nothing else will
                // ever consume it for this attempt now).
                let started_at = handles.connect_started_at.write().unwrap().remove(&node);
                if let Some(started_at) = started_at {
                    let attempt_ms = started_at.elapsed().as_millis() as u64;
                    super::conn_timing::log_timeout(&node, attempt_ms);
                }
                // Guard on the exact `Peer` snapshot read above (not just
                // `node`): a fresh reconnect can already have replaced it
                // with a new, healthy peer by the time this fires, and an
                // unconditional-by-NodeId cleanup here would silently delete
                // that live registration while `connection_states` (marked
                // `Connected` by the new peer's own DC-open handler) is
                // never touched -- a permanent "Node not found" with no
                // close/state-change log to explain it.
                match peer_opt {
                    Some(peer) => {
                        let expected = Arc::downgrade(&peer);
                        handles
                            .cleanup_session_if_current(
                                &node,
                                &expected,
                                true,
                                "watchdog_connect_timeout",
                            )
                            .await;
                    }
                    // No live peer to protect -- safe to clear whatever
                    // stale bookkeeping remains for `node` unconditionally.
                    None => {
                        handles
                            .cleanup_session_with_reason(&node, true, "watchdog_connect_timeout")
                            .await;
                    }
                }
                tracing::warn!(
                    "[WebRTC DC Zombie] detected: connection timeout before ReliableOrdered data channel opened for {} (attempt={})",
                    node,
                    attempt_id
                );
            }
        });
    }

    /// Offer resend, initiator side: schedules up to `super::OFFER_RESEND_MAX`
    /// resends of `node`'s just-sent, still-unanswered Offer, at
    /// `super::OFFER_RESEND_SCHEDULE_MS` (plus per-attempt jitter, see
    /// `super::offer_resend_jitter_ms`) after it was originally sent. Called
    /// only from `connection::connect_inner`, right after its own
    /// `send_offer` call succeeds -- i.e. only for a fresh connection
    /// attempt, never for a renegotiation/ICE-restart offer on an already
    /// established session (`send_offer` is shared by both, but only
    /// `connect_inner` opts into resend).
    ///
    /// Before each resend this re-checks three things and stops silently (no
    /// teardown, no error) the moment any of them no longer holds -- each is
    /// independent evidence that a resend would be pointless or wrong:
    /// - `attempt_id` is still `node`'s current entry in
    ///   `connection_attempt_ids`: a superseding attempt (a fresh
    ///   `connect_inner`, a takeover, ...) always reserves a brand-new id, so
    ///   a mismatch here means this attempt has already been superseded.
    /// - `node` still has a live entry in `peers`.
    /// - that peer's `signaling_state()` is still `HaveLocalOffer`: anything
    ///   else means an answer already arrived (negotiation moved on to
    ///   `Stable`) or some other negotiation step is in flight, either of
    ///   which makes a resend redundant or actively harmful.
    ///
    /// The resend itself never creates a new offer (no new ufrag/DTLS
    /// fingerprint) -- it re-publishes whatever `local_description()`
    /// currently holds, the exact same SDP `send_offer` already applied and
    /// sent, so the receiver's duplicate-offer idempotency
    /// (`signaling::duplicate_offer_decision`) can recognize it as a
    /// byte-identical re-delivery rather than a fresh renegotiation attempt.
    /// A send failure here is logged and otherwise ignored -- the remaining
    /// schedule (and, ultimately, the unchanged 6s connect watchdog) is the
    /// recovery path, not this task tearing anything down itself.
    pub(crate) fn spawn_offer_resend(&self, node: NodeId, attempt_id: u32, peer: Arc<Peer>) {
        let handles = self.peer_handles();

        tokio::spawn(async move {
            let mut elapsed_ms = 0u64;
            for (i, &scheduled_ms) in super::OFFER_RESEND_SCHEDULE_MS.iter().enumerate() {
                let jitter_ms =
                    super::offer_resend_jitter_ms(&handles.local_node_id, &node, i as u32);
                let sleep_ms = scheduled_ms.saturating_sub(elapsed_ms) + jitter_ms;
                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                elapsed_ms = scheduled_ms;

                let is_current_attempt = {
                    let lock = handles.connection_attempt_ids.read().unwrap();
                    matches!(lock.get(&node), Some(id) if *id == attempt_id)
                };
                if !is_current_attempt {
                    return;
                }

                let current_peer = {
                    let lock = handles.peers.read().await;
                    lock.get(&node).cloned()
                };
                let Some(current_peer) = current_peer else {
                    return;
                };
                if !Arc::ptr_eq(&current_peer, &peer) {
                    return;
                }

                // Held while re-reading state and the description to resend,
                // mirroring `send_offer`/`apply_offer`'s own use of this lock
                // for their whole check-then-act sequence -- see
                // `Peer::negotiating`'s doc comment for the race this closes
                // (an answer applying, or another negotiation step starting,
                // between the plain state check above and the read below).
                let _negotiating = current_peer.negotiating.lock().await;
                if current_peer.pc.signaling_state() != RTCSignalingState::HaveLocalOffer {
                    return;
                }
                let Some(local_desc) = current_peer.pc.local_description().await else {
                    return;
                };

                let resend_n = i as u32 + 1;
                tracing::info!(
                    "[OfferResend] resending unanswered offer to {} (attempt={}, resend={})",
                    node,
                    attempt_id,
                    resend_n
                );
                let msg = MessageContent::Data(SignalingData {
                    sender_id: handles.local_node_id.clone(),
                    receiver_id: node.clone(),
                    room_id: handles.room_id.clone(),
                    data: local_desc.sdp,
                    signaling_type: SignalingType::Offer,
                });
                if let Err(err) = handles.signaler.send_signaling(&node, msg).await {
                    tracing::warn!(
                        "[OfferResend] failed to resend offer to {} (attempt={}, resend={}/{}): {:?}",
                        node,
                        attempt_id,
                        resend_n,
                        super::OFFER_RESEND_MAX,
                        err
                    );
                }
            }
        });
    }

    pub(super) fn ensure_session_sweeper(&self) {
        if self.sweeper_started.swap(true, Ordering::SeqCst) {
            return;
        }

        let cancel = CancellationToken::new();
        {
            let mut lock = self.sweeper_cancel.lock().unwrap();
            *lock = Some(cancel.clone());
        }

        let handles = self.peer_handles();
        let cancel_for_task = cancel.clone();
        // Captured before the spawn so per-instance env overrides (read once
        // at construction -- see `WebRtcTransport::{reconnect_cooldown_ms,
        // disconnected_grace_ms}`) apply here without needing `&self` inside
        // the spawned loop. `last_disconnect_at`'s TTL intentionally mirrors
        // the reconnect cooldown itself: an entry only needs to live long
        // enough to gate the next `connect()` attempt for the same peer.
        let last_disconnect_ttl_ms = self.reconnect_cooldown_ms;
        let disconnected_grace_ms = self.disconnected_grace_ms;
        // Remote-takeover fix: swept independently of `last_disconnect_at`,
        // on its own TTL (`REMOTE_TAKEOVER_MIN_INTERVAL_MS`, the same window
        // `takeover_allowed`'s rate-limit guard uses) -- see
        // `WebRtcTransport::last_takeover_at`'s doc comment for why this
        // isn't threaded through `PeerSharedHandles` like the other per-peer
        // maps here.
        let last_takeover_at = self.last_takeover_at.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_for_task.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(SWEEPER_INTERVAL_MS)) => {}
                }

                {
                    let ttl = Duration::from_millis(last_disconnect_ttl_ms);
                    let mut lock = handles.last_disconnect_at.write().unwrap();
                    lock.retain(|_, at| at.elapsed() < ttl);
                }

                {
                    // Remote-takeover fix: bound `last_takeover_at` the same
                    // way `last_disconnect_at` is bounded just above -- an
                    // entry only needs to live long enough to gate the next
                    // takeover attempt for the same peer.
                    let ttl = Duration::from_millis(super::REMOTE_TAKEOVER_MIN_INTERVAL_MS);
                    let mut lock = last_takeover_at.write().unwrap();
                    lock.retain(|_, at| at.elapsed() < ttl);
                }

                {
                    // Repair-first ICE restart: bound `last_ice_restart_at`
                    // the same way `last_takeover_at` is bounded just above --
                    // see `WebRtcTransport::last_ice_restart_at`'s doc
                    // comment. Reached via `handles` (not a separately
                    // captured clone) because, unlike `last_takeover_at`,
                    // this map is threaded through `PeerSharedHandles`.
                    let ttl = Duration::from_millis(super::ICE_RESTART_MIN_INTERVAL_MS);
                    let mut lock = handles.last_ice_restart_at.write().unwrap();
                    lock.retain(|_, at| at.elapsed() < ttl);
                }

                {
                    // `[ConnTiming]` instrumentation: bound
                    // `disconnect_observed_at` independently of
                    // `last_disconnect_at`'s much shorter TTL -- see
                    // `WebRtcTransport::disconnect_observed_at`'s doc comment.
                    let ttl = Duration::from_millis(super::conn_timing::DISCONNECT_OBSERVED_TTL_MS);
                    let mut lock = handles.disconnect_observed_at.write().unwrap();
                    lock.retain(|_, at| at.elapsed() < ttl);
                }

                {
                    // Buffer-don't-drop fix: age out a buffer that never got
                    // a `connection_states` reservation at all -- see
                    // `WebRtcTransport::pending_candidates_first_seen`'s doc
                    // comment. A node that DOES go on to get a reservation is
                    // already covered by the per-node loop below (no-peer
                    // branch, grace expiry, watchdog), all of which clear
                    // this map alongside `pending_candidates`; that loop only
                    // ever iterates `connection_states`' keys, so it can
                    // never see a node that stayed unreserved the whole
                    // time. Read the stale set first, then re-check
                    // `connection_states` before actually removing anything,
                    // so a reservation that arrived in between is never
                    // wrongly clipped.
                    let ttl = Duration::from_millis(super::PENDING_CANDIDATE_UNRESERVED_TTL_MS);
                    let stale_unreserved: Vec<NodeId> = {
                        let first_seen = handles.pending_candidates_first_seen.read().await;
                        first_seen
                            .iter()
                            .filter(|(_, at)| at.elapsed() >= ttl)
                            .map(|(node, _)| node.clone())
                            .collect()
                    };
                    for node in stale_unreserved {
                        let still_unreserved = !handles
                            .connection_states
                            .read()
                            .unwrap()
                            .contains_key(&node);
                        if !still_unreserved {
                            continue;
                        }
                        handles.pending_candidates.write().await.remove(&node);
                        handles
                            .pending_candidates_first_seen
                            .write()
                            .await
                            .remove(&node);
                        tracing::warn!(
                            "[Sweeper] discarding stale unreserved pending ICE candidates for {} \
                             (no offer/answer arrived within {:?})",
                            node,
                            ttl
                        );
                    }
                }
                // `[ConnTiming]` instrumentation: make sure a pending
                // `dropped=<n>` summary for an already-expired rate-limit
                // window still gets flushed promptly during a quiet period,
                // not only whenever the next `[ConnTiming]` event happens to
                // occur.
                super::conn_timing::poll_dropped_summary();

                {
                    let peers_n = handles.peers.read().await.len();
                    let states_n = handles.connection_states.read().unwrap().len();
                    let pending_n = handles.pending_candidates.read().await.len();
                    let last_disc_n = handles.last_disconnect_at.read().unwrap().len();
                    crate::mem::log_mem_tick(peers_n, states_n, pending_n, last_disc_n);
                }

                let nodes = {
                    let lock = handles.connection_states.read().unwrap();
                    lock.keys().cloned().collect::<Vec<_>>()
                };

                for node in nodes {
                    let peer_opt = {
                        let lock = handles.peers.read().await;
                        lock.get(&node).cloned()
                    };

                    let Some(peer) = peer_opt else {
                        // Sweeper livelock fix: a bare reservation with
                        // nothing yet in `self.peers` can legitimately still
                        // be queued on `acquire_handshake_permit` (no
                        // timeout of its own, only 6 concurrent handshakes
                        // process-wide) -- reaping it here on the very first
                        // tick that observes it, as this branch used to,
                        // raced the dial itself: `connect_inner`'s
                        // `has_active_session` check would then silently see
                        // the reservation gone once the permit finally
                        // arrived and return `Ok(())` as a no-op, and
                        // DNVE3's next balancer tick would just reissue
                        // `Connect` -- a permanent livelock recycling
                        // permits under load, with no log line anywhere to
                        // explain it. See `reservation_reap_allowed` and
                        // `WebRtcTransport::connecting_reserved_at`'s doc
                        // comment for the full mechanism.
                        let reserved_at = handles
                            .connecting_reserved_at
                            .read()
                            .unwrap()
                            .get(&node)
                            .copied();
                        if !reservation_reap_allowed(
                            reserved_at,
                            CONNECTING_RESERVATION_REAP_GATE_MS,
                        ) {
                            continue;
                        }
                        tracing::warn!(
                            "[Sweeper] reaping stale Connecting reservation for {} (no peer \
                             registered)",
                            node
                        );
                        {
                            let mut lock = handles.connection_states.write().unwrap();
                            lock.remove(&node);
                        }
                        {
                            let mut lock = handles.disconnected_since.write().unwrap();
                            lock.remove(&node);
                        }
                        {
                            let mut lock = handles.pc_connected_at.write().unwrap();
                            lock.remove(&node);
                        }
                        {
                            // Remote-takeover fix -- see
                            // `WebRtcTransport::established_at`'s doc
                            // comment.
                            let mut lock = handles.established_at.write().unwrap();
                            lock.remove(&node);
                        }
                        {
                            let mut lock = handles.pending_candidates.write().await;
                            lock.remove(&node);
                        }
                        {
                            let mut lock = handles.pending_candidates_first_seen.write().await;
                            lock.remove(&node);
                        }
                        {
                            let mut lock = handles.connecting_reserved_at.write().unwrap();
                            lock.remove(&node);
                        }
                        continue;
                    };

                    let pc_state = peer.pc.connection_state();
                    let has_required_data_channel = Self::has_required_data_channel(&peer).await;

                    // Disarm the DC-open zombie timer the moment the required
                    // (ReliableOrdered) data channel is confirmed open. This
                    // normally happens via the channel's own `on_open` handler
                    // (`peer.rs`), but that handler is one-shot in webrtc-rs
                    // and never fires again for a data channel that survived
                    // an ICE restart without needing to actually reopen --
                    // yet `RTCPeerConnectionState::Connected`'s handler
                    // re-arms `pc_connected_at` on *every* such recovery (see
                    // `recover_connected_from_grace`'s doc comment), including
                    // this one. Left uncleared, that stale timestamp would
                    // sit in the map forever: the next time this same
                    // (perfectly healthy) channel's `ready_state()` reports
                    // anything other than `Open`, however transient, the
                    // elapsed-time check below is already far past
                    // `data_channel_open_timeout` and would immediately
                    // force-close an otherwise-healthy session.
                    if has_required_data_channel {
                        handles.pc_connected_at.write().unwrap().remove(&node);
                    }

                    let failed_or_closed = matches!(
                        pc_state,
                        RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                    );
                    let grace_snapshot = handles
                        .disconnected_since
                        .read()
                        .unwrap()
                        .get(&node)
                        .copied();
                    let grace_decision = decide_grace_expiry(
                        grace_snapshot,
                        pc_state,
                        has_required_data_channel,
                        disconnected_grace_ms,
                    );
                    if grace_decision == GraceExpiryDecision::RecoverFalsePositive {
                        // The missed-PONG heuristic that started this grace
                        // (mistlib-core's `stats::ping`) only ever watches the
                        // best-effort `Unreliable` channel -- it never checked
                        // the real `RTCPeerConnection`. Confirmed healthy here
                        // (pc Connected + ReliableOrdered DC open), so this was
                        // a false positive: clear the grace and restore
                        // `Connected` instead of destroying a working session
                        // (see `recover_connected_from_grace`'s doc for why
                        // this is safe to reuse for a non-ICE-restart
                        // recovery too -- it only checks that a grace is
                        // pending).
                        tracing::warn!(
                            "[Sweeper] liveness false-positive suppressed for {}: pc is Connected \
                             and the required data channel is open, so the missed-PONG grace is \
                             being cleared instead of reaping the session",
                            node
                        );
                        handles.recover_connected_from_grace(&node);
                        continue;
                    }
                    let disconnected_grace_expired = grace_decision == GraceExpiryDecision::Reap;
                    let data_channel_open_timeout = Self::data_channel_open_timeout();
                    let missing_required_channel = !has_required_data_channel
                        && handles
                            .pc_connected_at
                            .read()
                            .unwrap()
                            .get(&node)
                            .is_some_and(|at| at.elapsed() >= data_channel_open_timeout);

                    if failed_or_closed || disconnected_grace_expired || missing_required_channel {
                        let close_reason = if missing_required_channel {
                            "sweeper_dc_timeout"
                        } else if disconnected_grace_expired {
                            "sweeper_disconnected_grace_expired"
                        } else {
                            "sweeper_pc_failed_closed"
                        };
                        if disconnected_grace_expired {
                            tracing::warn!("[Sweeper] disconnected grace expired for {}", node);
                        }
                        if missing_required_channel {
                            tracing::warn!(
                                "[WebRTC DC Zombie] detected: ReliableOrdered data channel did not open within {:?} after pc connected for {}",
                                data_channel_open_timeout,
                                node
                            );
                        }
                        // Guard on the exact `peer` snapshot inspected above
                        // (not just `node`): a fresh reconnect racing this
                        // sweep can already have installed a new, healthy
                        // peer under the same `NodeId` between the reads
                        // above and this cleanup. An unconditional-by-NodeId
                        // removal here would silently delete that live
                        // registration from `self.peers` while
                        // `connection_states` (already marked `Connected` by
                        // the new peer's own DC-open handler) is left
                        // untouched -- a permanent "Node not found" with no
                        // close/state-change log to explain it.
                        let expected = Arc::downgrade(&peer);
                        handles
                            .cleanup_session_if_current(&node, &expected, true, close_reason)
                            .await;
                        if missing_required_channel {
                            tracing::warn!(
                                "[WebRTC DC Zombie] recovered: cleaned zombie session for {}",
                                node
                            );
                        }
                        tracing::warn!("[Sweeper] Force cleaned session for {}", node);
                    }
                }
            }
        });
    }

    pub fn stop_session_sweeper(&self) {
        self.sweeper_started.store(false, Ordering::SeqCst);
        let cancel = {
            let mut lock = self.sweeper_cancel.lock().unwrap();
            lock.take()
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }
}
