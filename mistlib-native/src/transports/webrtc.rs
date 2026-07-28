use async_trait::async_trait;
use bytes::Bytes;
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::transport::{NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::api::API;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::signaling_state::RTCSignalingState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

/// Maps `mistlib_core::config::WebRtcConfig::ice_servers` (CONFIG.md-documented,
/// user-settable) into the `webrtc`-rs crate's `RTCIceServer` shape consumed by
/// `RTCConfiguration`. Kept as a pure function -- independent of any
/// `WebRtcTransport`/`RTCPeerConnection` state -- so the mapping is unit
/// testable on its own. An empty `configured` slice maps to an empty `Vec`,
/// i.e. a user who explicitly configures zero ICE servers is honored as-is;
/// the "default has one Google STUN entry" behavior comes from
/// `Config::new_default`, not from this function.
///
/// Unusable entries (no URLs, or turn/turns without credentials -- see
/// `IceServer::is_usable`) are dropped with a warning rather than forwarded:
/// webrtc-rs re-validates every entry inside `API::new_peer_connection`, so a
/// single bad entry would otherwise fail *every* `create_pc` call -- and with
/// it every connection attempt -- for the rest of the session.
pub(crate) fn map_ice_servers(configured: &[mistlib_core::config::IceServer]) -> Vec<RTCIceServer> {
    configured
        .iter()
        .filter(|server| {
            if server.is_usable() {
                true
            } else {
                tracing::warn!(
                    "ignoring unusable ICE server entry {:?}: turn/turns URLs require a \
                     non-empty username and credential",
                    server.urls
                );
                false
            }
        })
        .map(|server| RTCIceServer {
            urls: server.urls.clone(),
            username: server.username.clone().unwrap_or_default(),
            credential: server.credential.clone().unwrap_or_default(),
        })
        .collect()
}

/// Upper bound on buffered-but-not-yet-appliable ICE candidates per node
/// (candidates that arrive before the remote description is set). Mirrors
/// `mistlib_wasm::transport::webrtc::pending_candidates::MAX_PENDING_CANDIDATES_PER_NODE`
/// so native and wasm apply the same bounded-memory behavior instead of
/// letting a slow/stuck handshake accumulate candidates unboundedly.
pub(crate) const MAX_PENDING_CANDIDATES_PER_NODE: usize = 64;

/// Buffer-don't-drop fix: upper bound on how many distinct nodes'
/// entries `pending_candidates` may hold at once. `handle_candidate` now
/// buffers a trickled Candidate even for a node with no `connection_states`
/// reservation yet (previously dropped outright -- see its doc comment for
/// the measured `watchdog_connect_timeout` regression this closes), so
/// `MAX_PENDING_CANDIDATES_PER_NODE`'s per-node cap alone no longer bounds
/// the map: a burst of candidates for many never-materializing nodes (e.g.
/// stale signaling for peers that already left the room) could otherwise
/// still grow the number of *keys* without limit even though each
/// individual node's list stays capped. A brand-new node's buffer is
/// refused outright once this cap is hit, rather than evicting another
/// node's buffer -- simpler, and no worse than the per-node cap's own
/// eviction: either way, the offer/answer that would unlock draining for
/// the refused entry hasn't arrived yet.
pub(crate) const MAX_PENDING_CANDIDATE_NODES: usize = 128;

/// Buffer-don't-drop fix: how long a node's buffered candidates may sit in
/// `pending_candidates` with no `connection_states` reservation at all
/// before the periodic sweeper discards them as abandoned (see
/// `pending_candidates_first_seen`'s doc comment and its sweep site in
/// `sweeper.rs`). A node that goes on to get a reservation drains (or is
/// reaped) through the ordinary per-node paths well before this fires in
/// practice; this only ages out a buffer for a node whose Offer/Answer never
/// arrived at all. No env override: like the other sweeper-timing
/// correctness guards in this module, this isn't an eval-harness tuning
/// knob.
#[cfg(test)]
pub(crate) const PENDING_CANDIDATE_UNRESERVED_TTL_MS: u64 = 50;
#[cfg(not(test))]
pub(crate) const PENDING_CANDIDATE_UNRESERVED_TTL_MS: u64 = 15_000;

/// Pushes `candidate` onto `list`, evicting the oldest entry once the count
/// exceeds [`MAX_PENDING_CANDIDATES_PER_NODE`]. Returns `true` if an entry was
/// evicted. Kept pure (no transport/lock state) so the eviction behavior is
/// unit-testable directly.
pub(crate) fn push_pending_candidate(list: &mut Vec<String>, candidate: String) -> bool {
    list.push(candidate);
    if list.len() > MAX_PENDING_CANDIDATES_PER_NODE {
        list.remove(0);
        true
    } else {
        false
    }
}

/// SPEC-13: whether `size` bytes crosses the "approaching the limit" warn
/// threshold (80% of `limit`). Factored out as a pure predicate, independent
/// of any transport state, so the boundary itself is unit-testable without
/// capturing `tracing` output. Cross-multiplied instead of using floating
/// point; safe from overflow because callers only reach this after already
/// confirming `size <= limit` (a `u32`).
pub(crate) fn exceeds_warn_threshold(size: usize, limit: u32) -> bool {
    size * 100 > limit as usize * 80
}

/// SPEC-13: same "lower ID wins" direction as the existing offer-glare rule
/// in `signaling::handle_offer` (`local_node_id.0 < remote_id.0`). Only the
/// initiator side attempts a one-shot ICE restart (an offer with
/// `ice_restart: true` on the existing `RTCPeerConnection`) when a peer's
/// disconnect grace period begins -- the other side has no PC-level trigger
/// of its own and instead sends a lightweight `RestartRequest` signaling
/// nudge asking the initiator to try (repair-first ICE restart, see
/// `PeerSharedHandles::start_disconnect_grace`/`send_restart_request` in
/// `peer.rs`). Kept pure so the direction is unit-testable without a real
/// `WebRtcTransport`/`RTCPeerConnection`.
pub(crate) fn is_ice_restart_initiator(local_id: &NodeId, remote_id: &NodeId) -> bool {
    local_id.0 < remote_id.0
}

/// Remote-takeover fix (see `signaling.rs`'s `SignalingType::Request` branch
/// and `handle_offer`): shared guard evaluation for both Change 1
/// (`CONNECT_REQUEST` arriving against a stale `self.peers` entry) and
/// Change 2 (a fresh-PC offer whose DTLS fingerprint differs from the
/// session already on file). Both are treated as evidence that the remote
/// side no longer has a working session with us -- this decides whether that
/// evidence is trustworthy enough to actually force our own stale session
/// down. Three independent classes of session are protected from a forced
/// takeover:
///
/// - `healthy` / `ms_since_connected` implement the **young healthy session
///   guard**: if the session was confirmed established
///   (`WebRtcTransport::established_at`) less than
///   `REMOTE_TAKEOVER_RECENT_CONNECT_MS` ago AND it still looks healthy
///   right now (`pc.connection_state() == Connected` and the required
///   ReliableOrdered data channel is open -- the same check the sweeper
///   uses), the incoming evidence is far more likely to be a stale/
///   re-delivered signaling message from the very attempt that already
///   succeeded than proof the remote actually lost its session, so takeover
///   is refused. An *unhealthy* session is never protected by this guard,
///   however recently it connected -- unhealthy plus incoming takeover
///   evidence is exactly the situation this whole mechanism exists to fix.
/// - `ms_since_connect_started` implements the **young in-flight attempt
///   guard**: refuses takeover while `connect_started_at` (an attempt still
///   between reserving a handshake permit and either the ReliableOrdered DC
///   opening or the connect watchdog firing) is younger than
///   `CONNECTION_TIMEOUT_MS`. Measured on a steady 50-node fleet with no
///   fault injection: the higher-ID side's `CONNECT_REQUEST_RETRY_INITIAL_MS`
///   (1s) retry routinely landed while the lower-ID/initiator side's own
///   dial to the very same peer was still mid-flight (fresh cross-host
///   handshakes take >1s at p90 under load) -- 1209 takeovers/30min recorded
///   as `remote_connect_request_takeover`, 537 of them fused into a
///   `connect_inner_error` within 300ms as the forced teardown raced the
///   about-to-succeed attempt's own negotiation. A young in-flight attempt
///   either completes on its own (p50=5ms) or the watchdog handles it; a
///   takeover in that window can only destroy work that was already
///   succeeding. Unlike the healthy-session guard above, this does not
///   require the session to look healthy yet -- mid-handshake is never
///   healthy by definition, which is exactly the case this guard exists to
///   protect. A session with no young in-flight attempt (the attempt already
///   resolved, or never started at all) remains takeover-eligible precisely
///   as before -- that stale-session case is what remote-takeover was built
///   to fix in the first place.
/// - `ms_since_last_takeover` implements the **rate limit**: at
///   most one forced takeover per peer per
///   `REMOTE_TAKEOVER_MIN_INTERVAL_MS`, tracked in
///   `WebRtcTransport::last_takeover_at`. This -- not the ordinary
///   reconnect cooldown (`last_disconnect_at`), which the takeover path
///   deliberately skips arming -- is the anti-storm mechanism for this path.
///
/// `None` for any `ms_since_*` means "no such timestamp recorded for this
/// peer" and is treated as "does not block": a peer that has never
/// (recently) connected, has no attempt currently in flight, or has never
/// (recently) been taken over, has nothing for that particular guard to
/// protect against.
///
/// Pure (no transport/lock/`RTCPeerConnection` state) so every guard
/// combination is exhaustively unit-testable without a live handshake -- see
/// `webrtc/tests/takeover.rs`.
pub(crate) fn takeover_allowed(
    healthy: bool,
    ms_since_connected: Option<u64>,
    ms_since_last_takeover: Option<u64>,
    ms_since_connect_started: Option<u128>,
) -> bool {
    let recently_connected_and_healthy =
        healthy && ms_since_connected.is_some_and(|ms| ms < REMOTE_TAKEOVER_RECENT_CONNECT_MS);
    if recently_connected_and_healthy {
        return false;
    }
    let young_in_flight_attempt =
        ms_since_connect_started.is_some_and(|ms| ms < CONNECTION_TIMEOUT_MS as u128);
    if young_in_flight_attempt {
        return false;
    }
    if ms_since_last_takeover.is_some_and(|ms| ms < REMOTE_TAKEOVER_MIN_INTERVAL_MS) {
        return false;
    }
    true
}

/// Reads an evaluation/tuning knob from the environment: `name`, parsed as a
/// positive `u64`, or `default` if unset/unparseable/zero. Read once at
/// `WebRtcTransport::new()` time (mirrors the existing
/// `MIST_WEBRTC_MAX_CONCURRENT_HANDSHAKES` handling a few lines below) and
/// stored as an instance field rather than re-read on every use -- these
/// knobs (`MIST_WEBRTC_RECONNECT_COOLDOWN_MS`, `MIST_WEBRTC_DISCONNECTED_GRACE_MS`,
/// `MIST_WEBRTC_CONNECTION_TIMEOUT_MS`) exist purely for the eval harness to
/// sweep values across load-test runs; production behavior is unchanged
/// (env unset) since every default matches the constant it replaces.
fn env_override_u64_ms(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// Best-effort recovery for a failed negotiation step on `pc`: if the failure
/// left signaling state anywhere other than `Stable`, issues a `rollback` on
/// whichever side actually has a pending description, clearing it back to
/// `Stable`. Mirrors `mistlib-wasm`'s `rollback_to_stable_on_failure`
/// (`transport/webrtc.rs`) -- native never had an equivalent, so a rejected
/// `set_local_description`/`set_remote_description`, or a `send_signaling`
/// that fails *after* a local offer/answer was already applied (e.g. the
/// `RoutedSignaler` returning `RouteNotFound` for a peer whose overlay route
/// hasn't caught up with a just-established connection yet), left the peer
/// wedged in `HaveLocalOffer`/`HaveRemoteOffer` forever: every later
/// negotiation attempt for that peer (a renegotiation, an ICE restart, the
/// remote's own offer) would keep failing the "signaling state is not
/// stable"/glare precondition, since nothing else ever moved it back to
/// `Stable`. Call this from every fallible step in `send_offer`/`apply_offer`/
/// `handle_answer` so a transient failure self-heals instead of requiring the
/// whole peer to be torn down and reconnected from scratch.
///
/// Best-effort: the rollback call itself can fail too (e.g. the connection is
/// already closing) -- that's logged and swallowed, since the original error
/// is what the caller should act on.
pub(crate) async fn rollback_to_stable_on_failure(pc: &Arc<RTCPeerConnection>, remote_id: &NodeId) {
    let signaling_state = pc.signaling_state();
    match signaling_state {
        RTCSignalingState::HaveLocalOffer | RTCSignalingState::HaveLocalPranswer => {
            if let Err(err) = pc.set_local_description(rollback_description()).await {
                tracing::warn!(
                    "Rollback to stable (local) failed for {} after a negotiation error: {:?}",
                    remote_id,
                    err
                );
            }
        }
        RTCSignalingState::HaveRemoteOffer | RTCSignalingState::HaveRemotePranswer => {
            if let Err(err) = pc.set_remote_description(rollback_description()).await {
                tracing::warn!(
                    "Rollback to stable (remote) failed for {} after a negotiation error: {:?}",
                    remote_id,
                    err
                );
            }
        }
        RTCSignalingState::Stable | RTCSignalingState::Closed | RTCSignalingState::Unspecified => {}
    }
}

/// Builds a `Rollback`-typed `RTCSessionDescription`. `RTCSessionDescription`
/// has no public constructor for this variant (only `offer`/`answer`/
/// `pranswer`) and its `parsed` field is `pub(crate)` to the `webrtc` crate,
/// so a struct literal (even via `..Default::default()`) doesn't compile from
/// here -- go through its `Deserialize` impl instead, the same way
/// `parse_offer_payload`/`parse_answer_payload` already parse inbound SDP
/// payloads via `serde_json::from_str::<RTCSessionDescription>`. The literal
/// is fixed and always valid, so a parse failure would be a `webrtc`-crate
/// shape change this code needs to know about immediately.
fn rollback_description() -> RTCSessionDescription {
    serde_json::from_str(r#"{"type":"rollback","sdp":""}"#)
        .expect("rollback session description literal must always parse")
}

const SERVER_ID: &str = "server";
/// Default for the `connection_timeout_ms` instance field (per-attempt
/// connect watchdog) -- overridable via `MIST_WEBRTC_CONNECTION_TIMEOUT_MS`,
/// read once at construction (see `WebRtcTransport::new`).
const CONNECTION_TIMEOUT_MS: u64 = 6000;
const DATA_CHANNEL_OPEN_TIMEOUT_MS: u64 = 5000;
/// Default for the `reconnect_cooldown_ms` instance field (per-peer
/// re-`connect()` cooldown, also reused as the `last_disconnect_at` TTL) --
/// overridable via `MIST_WEBRTC_RECONNECT_COOLDOWN_MS`, read once at
/// construction (see `WebRtcTransport::new`).
const RECONNECT_COOLDOWN_MS: u64 = 3000;
const DEFAULT_MAX_CONCURRENT_HANDSHAKES: usize = 6;
/// `CONNECT_REQUEST` retry schedule (the higher-ID side's periodic nudge
/// asking the lower-ID/initiator peer to send a fresh offer -- see
/// `signaling::spawn_connect_request_retry`): starts at
/// `CONNECT_REQUEST_RETRY_INITIAL_MS`, grows by `CONNECT_REQUEST_RETRY_MULTIPLIER`
/// each retry, capped at `CONNECT_REQUEST_RETRY_MAX_INTERVAL_MS` (see
/// `backoff::exponential_backoff_ms`). Replaces the old fixed 1s interval --
/// a fixed short interval repeated by every peer in a "cluster disconnect"
/// resynchronizes into a retry storm instead of spreading load out.
///
/// The cap was originally 10s, then lowered to 4s based on a load-test A/B
/// (fixed 1s baseline vs. 10s-cap backoff): the 10s cap improved the
/// typical case (p50/p90 recovery time, >60s rate) but *worsened* the worst
/// case (max recovery time 198s -> 316s) and the unrelated first-connect
/// `attempt_ms` tail (p90 1054ms -> 1394ms, p99 3938ms -> 4834ms), while the
/// timeout count and retry-chain-length distribution were unchanged between
/// the two runs -- i.e. the longer cap did not change whether a retry
/// eventually succeeds, only how long individual retries (and the
/// signaling-channel traffic they generate) get stretched out for a peer
/// that needs many of them. A lower cap keeps retries denser (closer to the
/// old cadence) for the rare, hard-to-recover peers without giving up the
/// spreading benefit that helped the common case.
const CONNECT_REQUEST_RETRY_INITIAL_MS: u64 = 1000;
const CONNECT_REQUEST_RETRY_MULTIPLIER: f64 = 1.5;
const CONNECT_REQUEST_RETRY_MAX_INTERVAL_MS: u64 = 4_000;
/// 10 sends (9 waited intervals) against the schedule above totals roughly
/// 28s -- in the same ~30s ballpark the old `30 x 1s` fixed schedule
/// occupied (see `backoff::tests::connect_request_default_schedule_totals_within_the_intended_ballpark`),
/// while sending far less often at the start of a cluster disconnect.
const DEFAULT_CONNECT_REQUEST_RETRIES: u32 = 10;
/// Default for the `disconnected_grace_ms` instance field (how long a peer
/// stays in `Reconnecting` -- e.g. mid ICE-restart -- before the sweeper
/// reaps it) -- overridable via `MIST_WEBRTC_DISCONNECTED_GRACE_MS`, read
/// once at construction (see `WebRtcTransport::new`). The architecture notes
/// documents `REORDER_GAP_TIMEOUT` (8s) as chosen to outlive *this default*
/// (5s) -- an env override changes a single process's own grace window, not
/// the documented default relationship.
#[cfg(test)]
pub(crate) const DISCONNECTED_GRACE_MS: u64 = 50;
#[cfg(not(test))]
pub(crate) const DISCONNECTED_GRACE_MS: u64 = 5000;

/// Remote-takeover fix: recent-connect guard threshold -- see
/// `takeover_allowed`'s doc comment for the full rationale. No env override:
/// this is a correctness guard, not an eval-harness tuning knob. Shrunk under
/// `#[cfg(test)]` (mirroring `DISCONNECTED_GRACE_MS` and friends above), but
/// kept generous (2s, not e.g. 50ms) relative to that -- unlike most of this
/// module's other test-only timings, an integration test for this guard
/// needs a real end-to-end handshake (real ICE/DTLS/SCTP, even if all
/// loopback) to complete and settle to `Connected` *before* the window
/// expires, and that has real, if small, wall-clock variance.
#[cfg(test)]
pub(crate) const REMOTE_TAKEOVER_RECENT_CONNECT_MS: u64 = 2_000;
#[cfg(not(test))]
pub(crate) const REMOTE_TAKEOVER_RECENT_CONNECT_MS: u64 = 5_000;
/// Remote-takeover fix: per-peer rate limit -- see `takeover_allowed`'s doc
/// comment for the full rationale. No env override: this is a correctness
/// guard, not an eval-harness tuning knob. Kept at the same, deliberately
/// generous `#[cfg(test)]` value as `REMOTE_TAKEOVER_RECENT_CONNECT_MS`
/// above rather than shrinking it further: every existing test only ever
/// needs to prove a *second* takeover attempt still falls inside the window
/// (never that the window has expired), so a larger value only adds safety
/// margin against slow/contended CI without costing any test real time
/// (nothing sleeps to cross it).
#[cfg(test)]
pub(crate) const REMOTE_TAKEOVER_MIN_INTERVAL_MS: u64 = 2_000;
#[cfg(not(test))]
pub(crate) const REMOTE_TAKEOVER_MIN_INTERVAL_MS: u64 = 10_000;

/// ICE agent timeouts: deliberately kept at webrtc-rs's own (pion-style)
/// defaults. An earlier repair-first iteration shortened these
/// (disconnected 2s / failed 10s / keepalive 500ms) to make `Disconnected`
/// an early repair trigger -- fault injection (4s cross-host UDP drop on a
/// 50-node fleet) proved that actively harmful in webrtc-rs:
///
/// - The ice crate marks a candidate pair Failed after
///   `DEFAULT_MAX_BINDING_REQUESTS = 7` unanswered binding requests and
///   NEVER pings a Failed pair again. Pair lifetime under total silence is
///   therefore ~7x the check cadence: ~14s at the default 2s keepalive, but
///   only ~3.5s at 500ms -- the shortened keepalive burned every pair
///   mid-blip, leaving zombie agents ("pingAllCandidates called with no
///   candidate pairs") that can never self-recover after the path returns.
/// - The intended rescue (in-place ICE restart) cannot reliably save a
///   wedged PC either: after any negotiation error webrtc-rs's SDP rollback
///   fails (`ErrPeerConnSDPTypeInvalidValue...`), the PC is stuck outside
///   `Stable`, and `try_ice_restart` is permanently rejected.
/// - Meanwhile the same injection showed natural recovery (keepalives
///   resume -> agent returns Connected -> `recover_connected_from_grace`)
///   repairs a surviving pair within ~1s of path restore, at zero cost.
///
/// So the winning configuration for blip tolerance is the default one:
/// pairs survive ~14s of silence, blips <= disconnected_timeout (5s) are
/// absorbed with no state change at all, and longer outages go through
/// grace -> (gated, degraded-only) restart -> teardown as designed. The env
/// overrides remain for experiments only. Full record:
/// the reconnect-latency investigation.
pub(crate) const ICE_DISCONNECTED_TIMEOUT_MS: u64 = 5_000;
/// See `ICE_DISCONNECTED_TIMEOUT_MS` -- webrtc-rs default, kept.
/// Overridable via `MIST_WEBRTC_ICE_FAILED_TIMEOUT_MS`.
pub(crate) const ICE_FAILED_TIMEOUT_MS: u64 = 25_000;
/// See `ICE_DISCONNECTED_TIMEOUT_MS` -- webrtc-rs default, kept. The check
/// cadence multiplies into pair lifetime under silence (7 unanswered checks
/// kill a pair), so shortening this is NOT a cheap detection win; it
/// directly shortens how long a blip a session can survive. Overridable via
/// `MIST_WEBRTC_ICE_KEEPALIVE_INTERVAL_MS`.
pub(crate) const ICE_KEEPALIVE_INTERVAL_MS: u64 = 2_000;

/// Repair-first ICE restart: per-peer rate limit shared by all three repair
/// triggers (`PeerSharedHandles::maybe_try_ice_restart`'s call sites -- the
/// ICE `Disconnected` state-change arm, a freshly-started `LivenessSuspect`
/// grace, and an incoming `RestartRequest`). Mirrors
/// `REMOTE_TAKEOVER_MIN_INTERVAL_MS`'s storm-protection role, but protects a
/// much cheaper action: a restart offer reuses the existing PC/DTLS cert and
/// is a harmless no-op renegotiation if the connection turns out to already
/// be healthy, so this can afford to be far more aggressive than the
/// takeover rate limit (2s vs. 10s in production) while still bounding how
/// often a genuinely flapping peer re-triggers the whole
/// create-offer/apply/send sequence. See `ice_restart_allowed`'s doc comment
/// for the decision itself and `WebRtcTransport::last_ice_restart_at` for how
/// the timestamp is tracked/swept.
#[cfg(test)]
pub(crate) const ICE_RESTART_MIN_INTERVAL_MS: u64 = 200;
#[cfg(not(test))]
pub(crate) const ICE_RESTART_MIN_INTERVAL_MS: u64 = 2_000;

/// Repair-first ICE restart: pure per-peer rate-limit decision backing
/// `PeerSharedHandles::maybe_try_ice_restart` -- see
/// `ICE_RESTART_MIN_INTERVAL_MS`'s doc comment for the rationale. Unlike
/// `takeover_allowed`, there is no analogous "recently connected" guard here:
/// a repair attempt on an already-healthy connection is harmless (an
/// in-place renegotiation on the same PC/cert), so the only thing worth
/// protecting against is triggering the whole create-offer/apply/send
/// sequence too often for the same peer. `None` (no restart ever recorded
/// for this peer) always allows; boundary is inclusive (`ms == threshold` is
/// allowed, matching `takeover_allowed`'s own strict-`<`-blocks convention).
/// Pure so the boundary is exhaustively unit-testable without a live
/// handshake -- see `webrtc/tests/ice_restart.rs`.
pub(crate) fn ice_restart_allowed(ms_since_last: Option<u64>) -> bool {
    ms_since_last.is_none_or(|ms| ms >= ICE_RESTART_MIN_INTERVAL_MS)
}

/// Repair-first ICE restart, storm-avoidance fix, later widened into ICE
/// restart's whole "rescue, not reflex" policy: how long
/// `PeerSharedHandles::spawn_repair_trigger`'s spawned repair task waits
/// before actually attempting a repair action (`maybe_try_ice_restart` on the
/// initiator side, `send_restart_request` on the non-initiator side), on top
/// of a deterministic per-pair jitter (`REPAIR_TRIGGER_JITTER_MS`, see
/// `repair_trigger_jitter_ms`).
///
/// Originally sized purely to survive a wake-race storm: measured via a 4s
/// SIGSTOP fault injection on a 50-node fleet, a process frozen for ~4s and
/// then resumed has its ICE agents' Disconnected-check fire BEFORE the
/// backlogged incoming packets are processed, so on wake it spuriously marks
/// ~all its peers Disconnected at once (a burst of grace starts observed
/// within ~70ms of SIGCONT). Without this debounce, `spawn_repair_trigger`
/// fired ~200 simultaneous ICE restarts (this side was the initiator for
/// every cross-host pair), renegotiating live sessions en masse and
/// destabilizing them -- both sides' graces then expired and ~200
/// otherwise-healthy sessions were torn down (a
/// `sweeper_disconnected_grace_expired` wave), for a blip the old,
/// conservative timeouts would have absorbed invisibly. The repair mechanism
/// amplified the failure instead of fixing it.
///
/// Widened again after a second measured fault injection (a 4s cross-host UDP
/// drop via iptables, same 50-node fleet, debounced build): the side that
/// honored ~200 `RestartRequest`s still fired ICE restarts at ~+2s -- while
/// the network path was still down -- because `ICE_DISCONNECTED_TIMEOUT_MS`
/// (2s) detection plus the old, short debounce+jitter (0.5s + <=0.3s) left
/// far too little of the grace window for natural recovery (keepalives
/// resume -> agent returns `Connected` -> `recover_connected_from_grace`) to
/// win the race. `create_offer(ice_restart: true)` discards the
/// about-to-recover candidate-pair state, so the fresh ufrag's connectivity
/// checks all failed into the still-blocked path -- converting recoverable
/// sessions into grace-expiry teardowns (127 of them in that run) instead of
/// fixing anything. The design conclusion this widening encodes: natural
/// recovery is the fast path and must get first claim on the grace window;
/// ICE restart is the rescue for what natural recovery cannot fix (NAT
/// rebind, path change), and should fire late rather than as an immediate
/// reflex.
///
/// This delay gives a self-recovering grace (the wake-race resolves as soon
/// as the backlog is processed; a `RestartRequest`-independent recovery can
/// also come from `recover_connected_from_grace` or a `clear_suspect`d
/// liveness suspect) time to clear `disconnected_since` before
/// `PeerSharedHandles::debounce_repair_trigger`'s re-check decides whether to
/// still act -- see that method's doc comment. Timing budget: detection
/// (`ICE_DISCONNECTED_TIMEOUT_MS`, 2s) + this debounce (2s) + jitter (<=
/// `REPAIR_TRIGGER_JITTER_MS`, 1s) means a restart only ever fires ~4-5s after
/// the actual failure began, and only if the grace is still pending (nothing
/// recovered on its own by then). That lands right at the edge of the
/// original `DISCONNECTED_GRACE_MS` (5s) window -- which is exactly why an
/// admitted restart re-arms its own peer's grace
/// (`PeerSharedHandles::rearm_disconnect_grace`, called from
/// `maybe_try_ice_restart`) instead of racing the sweeper with a clock that
/// had already been running since detection.
#[cfg(test)]
pub(crate) const REPAIR_TRIGGER_DEBOUNCE_MS: u64 = 20;
#[cfg(not(test))]
pub(crate) const REPAIR_TRIGGER_DEBOUNCE_MS: u64 = 2_000;

/// Repair-first ICE restart, storm-avoidance fix: upper bound (inclusive) on
/// the deterministic per-pair jitter added on top of
/// `REPAIR_TRIGGER_DEBOUNCE_MS` -- see that constant's doc comment for the
/// full rationale (including its own widening) and `repair_trigger_jitter_ms`
/// for the derivation. Spreads what would otherwise be a perfectly
/// synchronized burst (every peer of a frozen-then-resumed process detects
/// `Disconnected` within the same ~70ms window) across up to this many extra
/// milliseconds, so a fleet-wide wake-race doesn't fire its repair attempts
/// -- and the signaling traffic they generate -- in the same instant.
#[cfg(test)]
pub(crate) const REPAIR_TRIGGER_JITTER_MS: u64 = 10;
#[cfg(not(test))]
pub(crate) const REPAIR_TRIGGER_JITTER_MS: u64 = 1_000;

/// Repair-first ICE restart, storm-avoidance fix: deterministic per-pair
/// jitter in `0..=REPAIR_TRIGGER_JITTER_MS` derived by hashing
/// `(local_node_id, node)` with `std::hash::DefaultHasher` -- deliberately
/// not a random source, so the same pair always waits the same extra amount
/// and behavior stays reproducible across runs (mirrors
/// `mistlib_core::storage::types::deterministic_roll`'s "deterministic
/// pseudo-random" pattern). Only needs to spread a synchronized burst across
/// the jitter window, not be cryptographically meaningful, so a plain
/// non-cryptographic hasher is the right tool here.
pub(crate) fn repair_trigger_jitter_ms(local_node_id: &NodeId, node: &NodeId) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    local_node_id.hash(&mut hasher);
    node.hash(&mut hasher);
    hasher.finish() % (REPAIR_TRIGGER_JITTER_MS + 1)
}

/// Repair-first ICE restart, Change 3: `SignalingData.data` value that marks
/// a `SignalingType::Request` message as a `RestartRequest` repair nudge
/// rather than the ordinary CONNECT_REQUEST connection-initiation hint
/// (whose `data` is always the empty string -- see
/// `WebRtcTransport::send_connect_request`).
///
/// `mistlib_core::signaling::SignalingType` is a single enum defined in
/// `mistlib-core` and matched exhaustively (no wildcard arm) by both
/// `mistlib-native` and `mistlib-wasm`'s `SignalingHandler::handle_message`
/// impls, plus `mistlib-core`'s own Nostr session logic -- adding a genuine
/// new variant for this would require editing all three, which this fix is
/// deliberately scoped (native-only wire) to avoid. `SignalingData.data` is
/// otherwise unused by every production `Request` message (always
/// `String::new()`, see `send_connect_request` and
/// `WebRtcTransport::announce_to_room`), so this repurposes that already-free-
/// form field as a native-only, backward-compatible discriminator instead: a
/// peer that doesn't know about `RestartRequest` still receives a
/// normal-looking `Request` message and falls through to the ordinary
/// CONNECT_REQUEST handling path in `WebRtcTransport::handle_message`, which
/// never validated `data` being empty -- so an un-upgraded peer drops the
/// unknown marker harmlessly (worst case: it treats an ICE-restart nudge as
/// a spurious CONNECT_REQUEST, which is itself a no-op once a session already
/// exists).
pub(crate) const RESTART_REQUEST_MARKER: &str = "restart";

/// Pure predicate wrapping the `RESTART_REQUEST_MARKER` comparison -- used at
/// the receive side (`WebRtcTransport::handle_message`'s
/// `SignalingType::Request` arm) so the "is this a `RestartRequest`?" check is
/// unit-testable independent of a live `SignalingHandler`/transport, and so
/// the two sides of the round trip (`PeerSharedHandles::send_restart_request`
/// setting `data: RESTART_REQUEST_MARKER.to_string()`, this reading it back)
/// share one definition of what the marker means instead of comparing the
/// literal at each call site.
pub(crate) fn is_restart_request(data: &str) -> bool {
    data == RESTART_REQUEST_MARKER
}

/// Offer resend (initiator side): bounded retransmission schedule for an
/// initiator's Offer that hasn't been answered yet -- see
/// `sweeper::spawn_offer_resend`'s doc comment for the full mechanism. Values
/// are milliseconds elapsed since the offer was originally sent by
/// `connection::send_offer` (i.e. absolute offsets, not additive gaps between
/// resends): the schedule fires at `OFFER_RESEND_SCHEDULE_MS[0]` and again at
/// `OFFER_RESEND_SCHEDULE_MS[1]`. Overlay-routed signaling is fire-and-forget
/// past the first enqueue (`Signaler::send_signaling` returning `Ok` only
/// means "queued", not "delivered") and a churning hop's send queue or a
/// briefly stale routing table can silently drop the actual Offer -- trickled
/// ICE candidates already retry/buffer (see `handle_candidate`'s doc
/// comment), but before this fix the Offer/Answer exchange itself had no such
/// recovery, leaving the initiator stuck in `HaveLocalOffer` until the 6s
/// connect watchdog (`CONNECTION_TIMEOUT_MS`) killed the whole attempt. Kept
/// well inside that watchdog window so a successful resend still has time to
/// complete the handshake before the backstop fires; the watchdog remains
/// unchanged and is still the final backstop if every resend is also lost.
#[cfg(test)]
pub(crate) const OFFER_RESEND_SCHEDULE_MS: [u64; 2] = [30, 60];
#[cfg(not(test))]
pub(crate) const OFFER_RESEND_SCHEDULE_MS: [u64; 2] = [1_500, 3_000];

/// Number of resends `OFFER_RESEND_SCHEDULE_MS` schedules -- always matches
/// its length; kept as its own named constant purely so call sites/log lines
/// don't need to spell out `OFFER_RESEND_SCHEDULE_MS.len()`.
pub(crate) const OFFER_RESEND_MAX: u32 = OFFER_RESEND_SCHEDULE_MS.len() as u32;

/// Offer resend: upper bound (inclusive) on the deterministic per-attempt
/// jitter added on top of each `OFFER_RESEND_SCHEDULE_MS` entry -- see
/// `offer_resend_jitter_ms` for the derivation. Spreads what would otherwise
/// be perfectly synchronized resends (every initiator in a burst of
/// simultaneous fresh connects schedules the same two offsets) across up to
/// this many extra milliseconds, mirroring `REPAIR_TRIGGER_JITTER_MS`'s
/// anti-storm role for the ICE-restart repair trigger.
#[cfg(test)]
pub(crate) const OFFER_RESEND_JITTER_MS: u64 = 5;
#[cfg(not(test))]
pub(crate) const OFFER_RESEND_JITTER_MS: u64 = 300;

/// Offer resend: deterministic per-(peer, resend index) jitter in
/// `0..=OFFER_RESEND_JITTER_MS`, derived the same way
/// `repair_trigger_jitter_ms` derives its jitter -- hashing the inputs with
/// `std::hash::DefaultHasher` rather than drawing from a real random source,
/// so the same attempt always waits the same extra amount and behavior stays
/// reproducible across runs/replays. `resend_index` (0-based: which entry of
/// `OFFER_RESEND_SCHEDULE_MS` this is) is folded into the hash so the two
/// resends of the same attempt don't share identical jitter.
pub(crate) fn offer_resend_jitter_ms(
    local_node_id: &NodeId,
    node: &NodeId,
    resend_index: u32,
) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    local_node_id.hash(&mut hasher);
    node.hash(&mut hasher);
    resend_index.hash(&mut hasher);
    hasher.finish() % (OFFER_RESEND_JITTER_MS + 1)
}

/// What triggered a peer's current reconnect-grace period. `ClearSuspect` may
/// only cancel a `LivenessSuspect`-origin grace: an `Ice`-origin one is left
/// alone for ICE's own recovery signal to end (see `PeerSharedHandles::clear_suspect`).
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

pub(crate) mod backoff;
pub(crate) mod conn_timing;
pub mod connection;
pub mod peer;
pub mod publish;
pub mod signaling;
pub mod stats;
pub mod sweeper;

pub use peer::{MediaTrackEvent, Peer};
pub use stats::SctpPeerStats;

use peer::PeerSharedHandles;

/// Registers exactly Opus (audio) + H264 (video) on `engine`, in place of
/// `MediaEngine::register_default_codecs()` (which also registers
/// VP8/VP9/AV1/H265/G722/PCMU/PCMA). See the comment at the `new()` call
/// site for why the answer side must be pinned to this set.
///
/// The parameters (fmtp lines, payload types, RTCP feedback) below are
/// copied verbatim from `register_default_codecs()` in webrtc-rs 0.13.0
/// (src/api/media_engine/mod.rs) so the negotiated codec profiles are
/// identical to what pion/webrtc-rs peers already expect -- only the
/// VP8/VP9/AV1/H265/G722/PCMU/PCMA entries from that function are omitted.
fn register_h264_opus_codecs(engine: &mut MediaEngine) -> webrtc::error::Result<()> {
    engine.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;

    let video_rtcp_feedback = vec![
        RTCPFeedback {
            typ: "goog-remb".to_owned(),
            parameter: "".to_owned(),
        },
        RTCPFeedback {
            typ: "ccm".to_owned(),
            parameter: "fir".to_owned(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "".to_owned(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "pli".to_owned(),
        },
    ];
    for codec in [
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"
                        .to_owned(),
                rtcp_feedback: video_rtcp_feedback.clone(),
            },
            payload_type: 102,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f"
                        .to_owned(),
                rtcp_feedback: video_rtcp_feedback.clone(),
            },
            payload_type: 127,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_owned(),
                rtcp_feedback: video_rtcp_feedback.clone(),
            },
            payload_type: 125,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f"
                        .to_owned(),
                rtcp_feedback: video_rtcp_feedback.clone(),
            },
            payload_type: 108,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032"
                        .to_owned(),
                rtcp_feedback: video_rtcp_feedback.clone(),
            },
            payload_type: 123,
            ..Default::default()
        },
    ] {
        engine.register_codec(codec, RTPCodecType::Video)?;
    }

    Ok(())
}

/// Process-wide media-track handler. With multi-room sessions there is one
/// [`WebRtcTransport`] per session, created whenever a room is joined -- a
/// consumer registering before (or between) joins can't reach those future
/// transports, so registration stashes the sender here and every new
/// transport inherits it at construction. `app::register_media_track_handler`
/// also applies it to already-running sessions.
pub(crate) static GLOBAL_MEDIA_TX: Mutex<Option<mpsc::UnboundedSender<MediaTrackEvent>>> =
    Mutex::new(None);

pub struct WebRtcTransport {
    pub signaler: Arc<dyn Signaler>,
    pub local_node_id: NodeId,
    pub api: API,
    pub peers: Arc<tokio::sync::RwLock<HashMap<NodeId, Arc<Peer>>>>,
    /// Secondary, synchronous mirror of `peers`' keyset -> that peer's
    /// `Peer::send_tx`, kept in lock-step at every site that inserts or
    /// removes a `peers` entry (`signaling::handle_offer`,
    /// `connection::replace_peer_and_close_old`,
    /// `PeerSharedHandles::cleanup_session_impl`, `remove_peer_if_current`,
    /// `close_all_peer_connections`). Exists solely so `try_enqueue_send`
    /// never depends on `peers` itself: `peers` is a `tokio::sync::RwLock`,
    /// whose write-preferring fairness fails EVERY `try_read()` the instant a
    /// writer is queued -- even though every writer here only ever holds the
    /// lock for a brief, non-blocking insert/remove -- which measured as
    /// ~1200 dropped overlay messages/min fleet-wide (50 nodes) despite no
    /// long-held lock anywhere. `send_queues` is a `std::sync::RwLock`
    /// instead: a blocking read here only ever waits for one of those same
    /// microsecond-scale critical sections, so it cannot fail the way
    /// `peers.try_read()` did. A read observing this map a moment
    /// out-of-sync with `peers` (see each insert/remove site's own comment
    /// for the exact ordering) is harmless: a stale sender either still
    /// points at a live queue or, once the corresponding `Peer` is dropped,
    /// drains into nothing -- the same "queue full / stuck disconnected"
    /// outcome an actually-gone peer already produces today.
    pub(crate) send_queues: Arc<StdRwLock<HashMap<NodeId, mpsc::Sender<peer::QueuedSend>>>>,
    pub event_handler: Mutex<Option<Arc<dyn NetworkEventHandler>>>,
    /// Sender for remote media track (audio/video) arrival events. `None` by
    /// default (data-channel-only mode); set via `set_media_track_handler`
    /// before connecting peers that are expected to carry media, e.g. by a
    /// WHIP/broadcast-facing consumer such as mistlib-media.
    pub(crate) media_tx: Mutex<Option<mpsc::UnboundedSender<MediaTrackEvent>>>,
    pub connection_states: Arc<StdRwLock<HashMap<NodeId, ConnectionState>>>,
    pub room_id: Arc<StdRwLock<String>>,
    pub pending_candidates: Arc<tokio::sync::RwLock<HashMap<NodeId, Vec<String>>>>,
    /// STUN/TURN servers used for every new `RTCPeerConnection` (see
    /// `connection::create_pc`). Defaults to a single Google STUN entry
    /// (matching the pre-config-wiring hardcoded behavior) and is overridden
    /// via `set_ice_servers` from `Config::webrtc.ice_servers` at session
    /// construction (`layers::native_l0::init::build_webrtc_transport`).
    pub ice_servers: Arc<StdRwLock<Vec<RTCIceServer>>>,
    pub max_connections: AtomicU32,
    /// SPEC-13: upper bound (bytes, post-envelope/pre-wire) enforced by
    /// `Transport::send` before a payload ever reaches a DataChannel.
    /// Defaults to 64KiB, matching `Config::limits.max_message_bytes`'s
    /// default, and is overridden via `set_max_message_bytes` from config at
    /// session construction (`layers::native_l0::init::build_webrtc_transport`).
    pub max_message_bytes: AtomicU32,
    /// Per-peer re-`connect()` cooldown (default [`RECONNECT_COOLDOWN_MS`],
    /// overridable via `MIST_WEBRTC_RECONNECT_COOLDOWN_MS`) -- also reused as
    /// the `last_disconnect_at` map's TTL by the periodic sweeper. Read once
    /// at construction; see `env_override_u64_ms`.
    pub(crate) reconnect_cooldown_ms: u64,
    /// How long a peer stays in `Reconnecting` (e.g. mid ICE-restart) before
    /// the sweeper reaps it (default [`DISCONNECTED_GRACE_MS`], overridable
    /// via `MIST_WEBRTC_DISCONNECTED_GRACE_MS`). Read once at construction;
    /// see `env_override_u64_ms`.
    pub(crate) disconnected_grace_ms: u64,
    /// Per-attempt connect watchdog timeout (default [`CONNECTION_TIMEOUT_MS`],
    /// overridable via `MIST_WEBRTC_CONNECTION_TIMEOUT_MS`). Read once at
    /// construction; see `env_override_u64_ms`.
    pub(crate) connection_timeout_ms: u64,
    pub connection_attempt_ids: Arc<StdRwLock<HashMap<NodeId, u32>>>,
    pub connect_request_attempt_ids: Arc<StdRwLock<HashMap<NodeId, u32>>>,
    /// Sweeper livelock fix: when a `connection_states[node] = Connecting`
    /// reservation was made -- inserted by both `Transport::connect` and
    /// `signaling::handle_offer`, at the exact point each inserts the
    /// reservation, i.e. BEFORE `acquire_handshake_permit` (`connection.rs`)
    /// resolves. That permit wait has no timeout of its own (only 6
    /// concurrent handshakes are allowed process-wide via
    /// `handshake_semaphore`), so under load a reservation can sit with no
    /// corresponding `self.peers` entry for a while, purely queued -- not
    /// abandoned. The periodic sweeper's no-peer-registered branch consults
    /// this to distinguish the two cases instead of reaping on sight (see
    /// `sweeper::reservation_reap_allowed` and its call site for the
    /// measured livelock this closes: a silent reap racing a queued dial
    /// made `connect_inner`'s own `has_active_session` check silently no-op
    /// once the permit finally arrived, and DNVE3's balancer just reissued
    /// `Connect` every tick forever). Cleared alongside the sibling
    /// per-attempt maps in every teardown path
    /// (`PeerSharedHandles::cleanup_session_impl`) and the full-clear path
    /// (`close_all_peer_connections`) -- overwritten, not read, on every
    /// fresh reservation for the same node, exactly like `connect_started_at`.
    pub(crate) connecting_reserved_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    pub pc_connected_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    /// Remote-takeover fix: when this peer's ReliableOrdered data channel
    /// most recently opened (i.e. `ConnectionState::Connected` was reached).
    /// Consulted only by `takeover_allowed`'s recent-connect guard. Unlike
    /// `pc_connected_at` -- a short-lived zombie timer that is *removed* the
    /// instant the data channel opens, since it exists purely to bound how
    /// long a pc-Connected-but-DC-not-open session may linger -- this
    /// deliberately persists past establishment, since the whole point of
    /// the guard is to still recognize "this session just finished
    /// connecting" for a while (`REMOTE_TAKEOVER_RECENT_CONNECT_MS`) after
    /// the moment `pc_connected_at` itself is cleared. Set in
    /// `peer::setup_dc_open_handler` (mirroring where `connect_started_at`
    /// is consumed for `[ConnTiming]`); cleared everywhere `pc_connected_at`
    /// is cleared so it cannot leak for a peer that disconnects and never
    /// returns. A stale entry left behind for an unhealthy/gone peer is
    /// harmless even so -- `takeover_allowed` only ever consults it when
    /// `healthy` is also true, and `healthy` is always a fresh, live read.
    pub established_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    pub handshake_semaphore: Arc<Semaphore>,
    pub handshake_permits: Arc<StdRwLock<HashMap<NodeId, OwnedSemaphorePermit>>>,
    pub last_disconnect_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    /// Remote-takeover fix: per-peer timestamp of the last forced takeover
    /// (Change 1's `remote_connect_request_takeover` or Change 2's
    /// `remote_new_offer_takeover`, both in `signaling.rs`), backing
    /// `takeover_allowed`'s per-peer rate limit
    /// (`REMOTE_TAKEOVER_MIN_INTERVAL_MS`). Only read/written from
    /// `signaling.rs` (which always holds `&self`), so unlike the other
    /// per-peer maps here this is never threaded into `PeerSharedHandles`.
    /// Swept on a `REMOTE_TAKEOVER_MIN_INTERVAL_MS` TTL by the periodic
    /// sweeper (mirrors `last_disconnect_at`'s own TTL sweep) so it cannot
    /// grow unboundedly across many peers that were each taken over once and
    /// never again.
    pub(crate) last_takeover_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    /// Repair-first ICE restart: per-peer timestamp of the last ICE-restart
    /// repair attempt that actually started (see `ice_restart_allowed`'s doc
    /// comment and `PeerSharedHandles::maybe_try_ice_restart`), backing the
    /// per-peer rate limit (`ICE_RESTART_MIN_INTERVAL_MS`) shared by all
    /// three trigger sites: the ICE `Disconnected` state-change arm, a
    /// freshly-started `LivenessSuspect` grace, and an incoming
    /// `RestartRequest`. Unlike `last_takeover_at` (only ever touched from
    /// `signaling.rs`, which always holds `&self`), this needs to be
    /// reachable from the synchronous `on_peer_connection_state_change`
    /// callback, which only closes over a `PeerSharedHandles` -- so, like
    /// `disconnected_since`, it is threaded through `PeerSharedHandles`
    /// instead of kept `WebRtcTransport`-only. Swept on its own
    /// `ICE_RESTART_MIN_INTERVAL_MS` TTL by the periodic sweeper, mirroring
    /// `last_takeover_at`'s own sweep.
    pub(crate) last_ice_restart_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    pub disconnected_since: Arc<StdRwLock<HashMap<NodeId, DisconnectGrace>>>,
    /// `[ConnTiming]` instrumentation (see `conn_timing`): attempt-start
    /// timestamp, recorded wherever a connection attempt is reserved
    /// (alongside `connection_attempt_ids` -- `connection::connect_inner` and
    /// `signaling::handle_offer`) and consumed (removed) once the
    /// ReliableOrdered data channel opens (`peer::setup_dc_open_handler`) or
    /// the connect watchdog force-cleans a still-`Connecting` session
    /// (`sweeper::spawn_connection_watchdog`). Cleaned up alongside
    /// `connection_attempt_ids` in every teardown path so it cannot leak for
    /// a peer whose attempt never resolves either way.
    pub connect_started_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    /// `[ConnTiming]` instrumentation: when a peer's disconnect was
    /// confirmed -- the same three sites that insert into
    /// `last_disconnect_at` (`peer::setup_connection_state_handler`'s
    /// Failed/Closed branch, `peer::setup_dc_close_handler`, and
    /// `PeerSharedHandles::cleanup_session_impl`). Unlike
    /// `last_disconnect_at` (a short `LAST_DISCONNECT_TTL_MS`-scale window),
    /// this deliberately persists until the next establishment for the same
    /// peer so `downtime_ms` can be computed even after a longer outage.
    /// Bounded independently of `last_disconnect_at`'s TTL: swept for entries
    /// older than `conn_timing::DISCONNECT_OBSERVED_TTL_MS` by the periodic
    /// sweeper and capped at `conn_timing::MAX_DISCONNECT_OBSERVED_ENTRIES`
    /// (oldest evicted on insert past the cap, see
    /// `conn_timing::insert_disconnect_observed`).
    pub disconnect_observed_at: Arc<StdRwLock<HashMap<NodeId, Instant>>>,
    /// Buffer-don't-drop fix: when `pending_candidates` first started
    /// buffering for a node that, at the time, had no `connection_states`
    /// reservation at all (a trickled Candidate that raced ahead of the
    /// Offer/Answer that would have created one -- see `handle_candidate`'s
    /// unknown-node branch). Consulted only by the periodic sweeper's
    /// independent age-based sweep for exactly that case: a node that DOES
    /// go on to get a reservation is already covered by the sweeper's
    /// ordinary per-node loop (no-peer branch, grace expiry, watchdog), all
    /// of which clear this alongside `pending_candidates`; a node that never
    /// materializes at all is never visited by that loop (it only iterates
    /// `connection_states`' keys), so this is what lets its stale buffer age
    /// out instead of sitting in `pending_candidates` forever. Also cleared
    /// on every successful drain (`signaling::apply_offer`/`handle_answer`)
    /// and the full-clear path (`close_all_peer_connections`).
    pub(crate) pending_candidates_first_seen: Arc<tokio::sync::RwLock<HashMap<NodeId, Instant>>>,
    pub next_connection_attempt_id: AtomicU32,
    pub isolation_recovery_epoch: Arc<std::sync::atomic::AtomicU64>,
    pub sweeper_started: AtomicBool,
    pub(crate) sweeper_cancel: Mutex<Option<CancellationToken>>,
    /// Local media tracks marked "published" via `publish_local_track`
    /// (`transports::webrtc::publish`): every currently-connected peer gets
    /// them attached (with renegotiation), and every future peer gets them
    /// automatically at connection setup (`connection::create_pc`), before
    /// its first offer/answer. Keyed by `TrackLocal::id()` so publishing the
    /// same id again just replaces the stored track. Mirrors mistlib-wasm's
    /// `WasmWebRtcTransport::local_tracks` (`published` bookkeeping) --
    /// see `mistlib-wasm/src/transport/webrtc.rs`.
    pub(crate) published_tracks: Arc<StdRwLock<HashMap<String, Arc<TrackLocalStaticRTP>>>>,
    /// Per-peer RTP senders created for published tracks, so
    /// `attach_published_tracks_to_peer` can skip a track already attached
    /// to a given peer and `unpublish_local_track` can find the sender to
    /// remove. Mirrors mistlib-wasm's `peer_senders`.
    pub(crate) published_senders: Arc<tokio::sync::RwLock<PublishedSenders>>,
}

/// Per-peer map of published-track RTP senders, keyed by peer then track id.
pub(crate) type PublishedSenders = HashMap<NodeId, HashMap<String, Arc<RTCRtpSender>>>;

impl WebRtcTransport {
    pub fn new(signaler: Arc<dyn Signaler>, local_node_id: NodeId) -> Self {
        let mut m = MediaEngine::default();
        // Answer-side codec pinning: register ONLY H264 (video) + Opus
        // (audio) instead of `register_default_codecs()`. That default set
        // also includes VP8/VP9/AV1/H265/G722/PCMU/PCMA, and browsers
        // (tc-chat via mistlib-wasm's `publish_local_track`) typically list
        // VP8 first in their offer's codec preference order. Since we are
        // the answerer, the codecs WE list in the answer are what the
        // browser is constrained to send -- if VP8 were present in our
        // answer, the browser could pick it, and this native peer only ever
        // relays H264 onward (to RTSP/AVPro on the VRChat side, which has no
        // VP8 decode path). Restricting the engine to H264 + Opus forces the
        // answer to advertise only codecs we can actually consume, so the
        // browser is left with H264 as its only viable video choice.
        // Data channels are unaffected by codec registration -- SCTP doesn't
        // negotiate through the MediaEngine.
        register_h264_opus_codecs(&mut m)
            .expect("registering a fixed H264+Opus codec set on a fresh MediaEngine cannot fail");

        // Wire up the interceptor pipeline (NACK generator/responder, RTCP
        // sender/receiver reports, receive-side TWCC) so RTCP feedback the
        // codecs above already advertise (`nack`/`nack pli` in
        // `register_h264_opus_codecs`) actually does something. Without a
        // registered interceptor chain -- the default before this change --
        // `APIBuilder` falls back to an empty `Registry` (see
        // `APIBuilder::build` in webrtc-rs), so lost inbound packets were
        // never NACKed and no retransmissions were ever served for outbound
        // ones; the relay could only mitigate loss by dropping until the
        // next IDR. `register_default_interceptors` must run after codec
        // registration (it registers additional feedback/header-extension
        // capabilities against the already-populated `MediaEngine`, mirroring
        // pion/webrtc-rs's own `NewAPI` example ordering).
        let mut registry = webrtc::interceptor::registry::Registry::new();
        registry =
            webrtc::api::interceptor_registry::register_default_interceptors(registry, &mut m)
                .expect(
                    "registering the default interceptor set (NACK, RTCP reports, TWCC) against a \
                 MediaEngine with only H264+Opus registered cannot fail",
                );

        // Repair-first ICE restart, Change 1: shorten the ICE agent's own
        // Disconnected/Failed/keepalive timeouts from webrtc-rs's pion-style
        // defaults (5s/25s/2s) so `RTCPeerConnectionState::Disconnected` --
        // this module's earliest repair trigger -- fires much sooner. See
        // `ICE_DISCONNECTED_TIMEOUT_MS`'s doc comment for why this is safe to
        // be aggressive about (a cheap, non-destructive repair trigger, not
        // the teardown decision). Applied once here to the single `API`
        // shared by every `RTCPeerConnection` this transport ever creates
        // (`connection::create_pc`'s `self.api.new_peer_connection(..)`) --
        // `SettingEngine` has no per-call knob, only this process-wide one.
        let ice_disconnected_timeout_ms = env_override_u64_ms(
            "MIST_WEBRTC_ICE_DISCONNECTED_TIMEOUT_MS",
            ICE_DISCONNECTED_TIMEOUT_MS,
        );
        let ice_failed_timeout_ms =
            env_override_u64_ms("MIST_WEBRTC_ICE_FAILED_TIMEOUT_MS", ICE_FAILED_TIMEOUT_MS);
        let ice_keepalive_interval_ms = env_override_u64_ms(
            "MIST_WEBRTC_ICE_KEEPALIVE_INTERVAL_MS",
            ICE_KEEPALIVE_INTERVAL_MS,
        );
        // Sanity-check the ordering the eval harness's env overrides must
        // preserve for these three knobs to make sense together (a keepalive
        // slower than the disconnected timeout could never fire in time to
        // prevent it; a disconnected timeout at or past the failed timeout
        // would never actually get observed as `Disconnected` first). Only a
        // `debug_assert!` (not a hard clamp): these three are eval-harness
        // tuning knobs read once at construction, not user-facing
        // `Config::webrtc` fields with their own validation layer, so a
        // misconfigured sweep is a bug in the harness's own invocation to
        // surface loudly in a debug build rather than something production
        // code should silently "fix" by clamping to a value the caller never
        // asked for.
        debug_assert!(
            ice_keepalive_interval_ms < ice_disconnected_timeout_ms
                && ice_disconnected_timeout_ms < ice_failed_timeout_ms,
            "ICE timeout knobs must satisfy keepalive < disconnected < failed \
             (got keepalive={ice_keepalive_interval_ms}ms, \
             disconnected={ice_disconnected_timeout_ms}ms, failed={ice_failed_timeout_ms}ms)"
        );
        let mut setting_engine = SettingEngine::default();
        setting_engine.set_ice_timeouts(
            Some(Duration::from_millis(ice_disconnected_timeout_ms)),
            Some(Duration::from_millis(ice_failed_timeout_ms)),
            Some(Duration::from_millis(ice_keepalive_interval_ms)),
        );

        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .build();

        let max_concurrent_handshakes = std::env::var("MIST_WEBRTC_MAX_CONCURRENT_HANDSHAKES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_CONCURRENT_HANDSHAKES);

        let reconnect_cooldown_ms =
            env_override_u64_ms("MIST_WEBRTC_RECONNECT_COOLDOWN_MS", RECONNECT_COOLDOWN_MS);
        let disconnected_grace_ms =
            env_override_u64_ms("MIST_WEBRTC_DISCONNECTED_GRACE_MS", DISCONNECTED_GRACE_MS);
        let connection_timeout_ms =
            env_override_u64_ms("MIST_WEBRTC_CONNECTION_TIMEOUT_MS", CONNECTION_TIMEOUT_MS);

        Self {
            signaler,
            local_node_id,
            api,
            peers: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            send_queues: Arc::new(StdRwLock::new(HashMap::new())),
            event_handler: Mutex::new(None),
            media_tx: Mutex::new(GLOBAL_MEDIA_TX.lock().unwrap().clone()),
            connection_states: Arc::new(StdRwLock::new(HashMap::new())),
            room_id: Arc::new(StdRwLock::new("lobby".to_string())),
            pending_candidates: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            // Mirrors `Config::new_default()`'s `webrtc.ice_servers`; `build_session`
            // overwrites it with the real config via `set_ice_servers`, so this only
            // matters for callers constructing the transport directly.
            ice_servers: Arc::new(StdRwLock::new(vec![RTCIceServer {
                urls: mistlib_core::config::DEFAULT_STUN_URLS
                    .iter()
                    .map(|u| u.to_string())
                    .collect(),
                ..Default::default()
            }])),
            max_connections: AtomicU32::new(30),
            max_message_bytes: AtomicU32::new(65536),
            reconnect_cooldown_ms,
            disconnected_grace_ms,
            connection_timeout_ms,
            connection_attempt_ids: Arc::new(StdRwLock::new(HashMap::new())),
            connect_request_attempt_ids: Arc::new(StdRwLock::new(HashMap::new())),
            connecting_reserved_at: Arc::new(StdRwLock::new(HashMap::new())),
            pc_connected_at: Arc::new(StdRwLock::new(HashMap::new())),
            established_at: Arc::new(StdRwLock::new(HashMap::new())),
            handshake_semaphore: Arc::new(Semaphore::new(max_concurrent_handshakes)),
            handshake_permits: Arc::new(StdRwLock::new(HashMap::new())),
            last_disconnect_at: Arc::new(StdRwLock::new(HashMap::new())),
            last_takeover_at: Arc::new(StdRwLock::new(HashMap::new())),
            last_ice_restart_at: Arc::new(StdRwLock::new(HashMap::new())),
            disconnected_since: Arc::new(StdRwLock::new(HashMap::new())),
            connect_started_at: Arc::new(StdRwLock::new(HashMap::new())),
            disconnect_observed_at: Arc::new(StdRwLock::new(HashMap::new())),
            pending_candidates_first_seen: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            next_connection_attempt_id: AtomicU32::new(1),
            isolation_recovery_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            sweeper_started: AtomicBool::new(false),
            sweeper_cancel: Mutex::new(None),
            published_tracks: Arc::new(StdRwLock::new(HashMap::new())),
            published_senders: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

    pub fn set_room_id(&self, room_id: String) {
        let mut room = self.room_id.write().unwrap();
        *room = room_id;
    }

    pub fn set_max_connections(&self, max: u32) {
        self.max_connections.store(max, Ordering::Relaxed);
    }

    /// SPEC-13: overrides the max payload size accepted by `Transport::send`
    /// (and, transitively, `broadcast`). A `set_config` call applies this
    /// best-effort -- it only affects sends issued after the store, not any
    /// already in flight.
    pub fn set_max_message_bytes(&self, max: u32) {
        self.max_message_bytes.store(max, Ordering::Relaxed);
    }

    /// SPEC-13: rejects a payload larger than `max_message_bytes` before it
    /// ever reaches a DataChannel. Checked first thing in `Transport::send`,
    /// ahead of the peer/channel lookup, so it applies even to a target with
    /// no live connection (see `transports/webrtc/tests/message_size.rs`) and
    /// so `broadcast` (which forwards to `send` per target) inherits it for
    /// free. Does not apply to the signaling (offer/answer/candidate) path,
    /// which never calls this.
    fn check_message_size(&self, size: usize) -> mistlib_core::error::Result<()> {
        let limit = self.max_message_bytes.load(Ordering::Relaxed);
        if size > limit as usize {
            return Err(mistlib_core::error::MistError::MessageTooLarge { size, limit });
        }
        if exceeds_warn_threshold(size, limit) {
            tracing::warn!("message size {size} bytes exceeds 80% of max_message_bytes ({limit})");
        }
        Ok(())
    }

    /// Overrides the STUN/TURN servers used by every subsequently-created
    /// `RTCPeerConnection`. Does not affect peers already connected.
    pub fn set_ice_servers(&self, servers: Vec<RTCIceServer>) {
        *self.ice_servers.write().unwrap() = servers;
    }

    /// Registers a channel to receive remote media track (audio/video) arrival
    /// events for peers connected from this point forward. Peers created before
    /// this call was made do not retroactively get the handler wired up.
    pub fn set_media_track_handler(&self, tx: mpsc::UnboundedSender<MediaTrackEvent>) {
        let mut media_tx = self.media_tx.lock().unwrap();
        *media_tx = Some(tx);
    }

    pub(crate) fn get_room_id(&self) -> String {
        self.room_id.read().unwrap().clone()
    }

    pub(crate) fn peer_handles(&self) -> PeerSharedHandles {
        PeerSharedHandles {
            connection_states: self.connection_states.clone(),
            peers: self.peers.clone(),
            send_queues: self.send_queues.clone(),
            pending_candidates: self.pending_candidates.clone(),
            connection_attempt_ids: self.connection_attempt_ids.clone(),
            connect_request_attempt_ids: self.connect_request_attempt_ids.clone(),
            connecting_reserved_at: self.connecting_reserved_at.clone(),
            pc_connected_at: self.pc_connected_at.clone(),
            established_at: self.established_at.clone(),
            handshake_permits: self.handshake_permits.clone(),
            last_disconnect_at: self.last_disconnect_at.clone(),
            last_ice_restart_at: self.last_ice_restart_at.clone(),
            disconnected_since: self.disconnected_since.clone(),
            connect_started_at: self.connect_started_at.clone(),
            disconnect_observed_at: self.disconnect_observed_at.clone(),
            pending_candidates_first_seen: self.pending_candidates_first_seen.clone(),
            signaler: self.signaler.clone(),
            isolation_recovery_epoch: self.isolation_recovery_epoch.clone(),
            room_id: self.get_room_id(),
            local_node_id: self.local_node_id.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) async fn cleanup_session(&self, node: &NodeId, force_failed: bool) {
        self.cleanup_session_with_reason(node, force_failed, "cleanup_session")
            .await;
    }

    pub(crate) async fn cleanup_session_with_reason(
        &self,
        node: &NodeId,
        force_failed: bool,
        reason: &'static str,
    ) {
        self.peer_handles()
            .cleanup_session_with_reason(node, force_failed, reason)
            .await;
        // Drop stale published-track sender bookkeeping for this node -- the
        // peer connection it referred to is gone, and `connection::create_pc`
        // also clears this defensively on the next reconnect, but doing it
        // here too keeps `published_senders` from accumulating dead entries
        // across many connect/disconnect cycles for nodes that never
        // reconnect.
        self.published_senders.write().await.remove(node);
    }

    /// Same as `cleanup_session_with_reason`, but only tears down `node` if
    /// `self.peers` still maps it to `expected` -- see
    /// `PeerSharedHandles::cleanup_session_if_current` for the race this
    /// closes. Used by the connect-timeout watchdog and the periodic
    /// sweeper, both of which act on an earlier `Peer` snapshot.
    pub(crate) async fn cleanup_session_if_current(
        &self,
        node: &NodeId,
        expected: &std::sync::Weak<Peer>,
        force_failed: bool,
        reason: &'static str,
    ) {
        let cleaned = self
            .peer_handles()
            .cleanup_session_if_current(node, expected, force_failed, reason)
            .await;
        if cleaned {
            // Only drop `published_senders` bookkeeping when this call
            // actually superseded the registration it was aimed at -- if
            // `node` now belongs to a newer, live peer, that peer's sender
            // bookkeeping must be left alone.
            self.published_senders.write().await.remove(node);
        }
    }

    pub async fn close_all_peer_connections(&self) {
        let peers = {
            let mut lock = self.peers.write().await;
            std::mem::take(&mut *lock)
        };
        self.send_queues.write().unwrap().clear();

        for (node, peer) in peers {
            tracing::warn!("[WebRTC Close] reason=room_close_all node={}", node);
            peer.close_all().await;
            crate::mem::record_peer_cleaned();
        }

        self.pending_candidates.write().await.clear();
        self.pending_candidates_first_seen.write().await.clear();
        self.connection_attempt_ids.write().unwrap().clear();
        self.connect_request_attempt_ids.write().unwrap().clear();
        self.connecting_reserved_at.write().unwrap().clear();
        self.pc_connected_at.write().unwrap().clear();
        self.established_at.write().unwrap().clear();
        self.handshake_permits.write().unwrap().clear();
        self.connection_states.write().unwrap().clear();
        self.last_disconnect_at.write().unwrap().clear();
        self.last_takeover_at.write().unwrap().clear();
        self.last_ice_restart_at.write().unwrap().clear();
        self.disconnected_since.write().unwrap().clear();
    }

    /// Pushes `data` onto a peer's ordered send queue (`Peer::send_tx`,
    /// drained by `Peer::spawn_send_queue`). Takes the queue sender directly
    /// -- rather than a `&Peer` -- so both producers can hand it whatever
    /// they resolved: the async `Transport::send` (an awaited read of
    /// `self.peers`) and `try_enqueue_send` (a blocking read of
    /// `self.send_queues`; see its doc comment) ultimately just need to push
    /// onto the same single-writer-per-peer queue.
    fn enqueue_on_peer(
        node: &NodeId,
        send_tx: &mpsc::Sender<peer::QueuedSend>,
        data: Bytes,
        method: DeliveryMethod,
    ) -> mistlib_core::error::Result<()> {
        send_tx
            .try_send(peer::QueuedSend { data, method })
            .map_err(|_| {
                mistlib_core::error::MistError::Internal(format!(
                    "Send queue full for {:?} method {:?} (peer not keeping up or stuck disconnected)",
                    node, method
                ))
            })
    }

    /// Fully synchronous version of `Transport::send` -- no `.await`
    /// anywhere, so it can be called inline from `MistEngine::handle_action_for`
    /// (`engine/action.rs`) without spawning a task for it. This is what
    /// makes the fix in `Peer::spawn_send_queue` actually hold end to end:
    /// that queue only preserves the order messages are *enqueued* in, so
    /// the enqueue call itself must happen synchronously, in the exact order
    /// `OverlayAction::SendMessage` actions were produced (overlay seq
    /// numbers are stamped synchronously too, in `OverlayRouter::wrap_data`)
    /// -- if this were spawned instead, N concurrently spawned enqueue calls
    /// could still run in a different order than they were spawned in
    /// (tokio's scheduler makes no such guarantee), which would silently
    /// reintroduce the exact reordering bug this queue exists to fix.
    ///
    /// Resolves the queue sender from `self.send_queues` (a blocking
    /// `std::sync::RwLock` read) instead of `.await`ing or `try_read()`-ing
    /// `self.peers`: see `send_queues`'s doc comment for why the latter is
    /// unsound for this specific caller -- a queued writer on the
    /// tokio `RwLock` fails every concurrent `try_read()`, even though our
    /// own writers only ever hold it for a brief, non-blocking swap.
    pub(crate) fn try_enqueue_send(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> mistlib_core::error::Result<()> {
        self.check_message_size(data.len())?;

        let send_queues = self.send_queues.read().unwrap();
        let send_tx = send_queues.get(node).ok_or_else(|| {
            mistlib_core::error::MistError::Internal(format!("Node not found: {:?}", node))
        })?;
        Self::enqueue_on_peer(node, send_tx, data, method)
    }

    /// Synchronous broadcast built on `try_enqueue_send` -- see its doc
    /// comment. Best-effort per target, exactly like the async `broadcast`.
    pub(crate) fn try_enqueue_broadcast(&self, data: Bytes, method: DeliveryMethod) {
        for target in self.get_connected_nodes() {
            let _ = self.try_enqueue_send(&target, data.clone(), method);
        }
    }
}

impl WebRtcTransport {
    /// シグナリングサーバーへ参加通知を送る。
    /// `start()` の後、ルームに参加する準備が整った時点で明示的に呼ぶこと。
    pub async fn announce_to_room(&self) -> mistlib_core::error::Result<()> {
        let room_id = self.get_room_id();
        self.signaler
            .send_signaling(
                &NodeId(SERVER_ID.to_string()),
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
}

#[async_trait]
impl Transport for WebRtcTransport {
    async fn start(
        &self,
        handler: Arc<dyn NetworkEventHandler>,
    ) -> mistlib_core::error::Result<()> {
        self.ensure_session_sweeper();
        let mut h = self.event_handler.lock().unwrap();
        *h = Some(handler);
        Ok(())
    }

    async fn send(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> mistlib_core::error::Result<()> {
        self.check_message_size(data.len())?;

        let peer = {
            let peers = self.peers.read().await;
            peers.get(node).cloned()
        }
        .ok_or_else(|| {
            mistlib_core::error::MistError::Internal(format!("Node not found: {:?}", node))
        })?;

        Self::enqueue_on_peer(node, &peer.send_tx, data, method)
    }

    async fn broadcast(
        &self,
        data: Bytes,
        method: DeliveryMethod,
    ) -> mistlib_core::error::Result<()> {
        let targets = self.get_connected_nodes();
        for target in targets {
            let _ = self.send(&target, data.clone(), method).await;
        }
        Ok(())
    }

    fn get_connection_state(&self, node: &NodeId) -> ConnectionState {
        let states = self.connection_states.read().unwrap();
        states
            .get(node)
            .cloned()
            .unwrap_or(ConnectionState::Disconnected)
    }

    async fn connect(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        {
            let peers = self.peers.read().await;
            if peers.contains_key(node) {
                return Ok(());
            }
        }

        if self.local_node_id.0 > node.0 {
            tracing::debug!(
                "[Glare] requesting lower-id peer {} to initiate WebRTC offer",
                node
            );
            return self.request_lower_id_offer(node).await;
        }

        let wait_duration = {
            let last_disconnect = self.last_disconnect_at.read().unwrap();
            last_disconnect.get(node).copied().and_then(|at| {
                let elapsed = at.elapsed();
                let cooldown = Duration::from_millis(self.reconnect_cooldown_ms);
                if elapsed < cooldown {
                    Some(cooldown - elapsed)
                } else {
                    None
                }
            })
        };

        if let Some(wait_duration) = wait_duration {
            tracing::warn!(
                "[Reconnect] waiting {:?} before retrying connection to {}",
                wait_duration,
                node
            );
            tokio::time::sleep(wait_duration).await;
        }

        {
            let mut states = self.connection_states.write().unwrap();
            if states.contains_key(node) {
                return Ok(());
            }
            let max = self.max_connections.load(Ordering::Relaxed) as usize;
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
            states.insert(node.clone(), ConnectionState::Connecting);
            tracing::debug!("[CS] INSERT connect: {} total={}", node, states.len());
        }
        // Sweeper livelock fix: record the reservation timestamp at the
        // exact moment the reservation itself is made, before
        // `connect_inner` ever reaches `acquire_handshake_permit` -- see
        // `connecting_reserved_at`'s doc comment.
        self.connecting_reserved_at
            .write()
            .unwrap()
            .insert(node.clone(), Instant::now());

        let result = self.connect_inner(node).await;
        if result.is_err() {
            let states = self.connection_states.read().unwrap();
            tracing::debug!("[CS] FAILED connect_err: {} total={}", node, states.len());
        }
        result
    }

    async fn disconnect(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        self.cleanup_session_with_reason(node, false, "explicit_disconnect")
            .await;
        Ok(())
    }

    async fn suspect_disconnected(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        if !self.peer_handles().mark_suspect_disconnected(node) {
            tracing::debug!(
                "[CS] ignored suspect-disconnected for {} (not Connected, or already in grace)",
                node
            );
        }
        Ok(())
    }

    async fn clear_suspect(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        if !self.peer_handles().clear_suspect(node) {
            tracing::debug!(
                "[CS] ignored clear-suspect for {} (no liveness-suspect grace active)",
                node
            );
        }
        Ok(())
    }

    fn get_connected_nodes(&self) -> Vec<NodeId> {
        let states = self.connection_states.read().unwrap();
        states
            .iter()
            .filter(|(_, &s)| s == ConnectionState::Connected)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

impl WebRtcTransport {
    pub fn get_active_connection_states(&self) -> Vec<(NodeId, ConnectionState)> {
        let states = self.connection_states.read().unwrap();
        states
            .iter()
            .filter(|(_, s)| {
                matches!(
                    **s,
                    ConnectionState::Connected
                        | ConnectionState::Connecting
                        | ConnectionState::Reconnecting
                )
            })
            .map(|(id, s)| (id.clone(), *s))
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests;
