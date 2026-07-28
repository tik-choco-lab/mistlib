//! `[ConnTiming]` connection-timing instrumentation for the WebRTC transport:
//! measures connection-establishment time, reconnection downtime, and
//! connection-attempt timeouts, and logs one `tracing::info!` line per event
//! in a fixed, machine-parseable format so an external tool (the eval
//! harness) can grep and parse these lines without ambiguity.
//! The exact formats (key order, kind values) are a contract with that
//! parser -- do not reorder or rename fields without updating it.
//!
//! Deliberately kept free of any `WebRtcTransport`/lock/`RTCPeerConnection`
//! state: everything here is a pure function or a small self-contained
//! struct, so the formatting and rate-limiting logic are exhaustively
//! unit-testable on their own, independent of a real WebRTC handshake.
//!
//! Note the emitted lines never contain the literal `[CS]` tag -- the log
//! layer filters lines containing it, so `[ConnTiming]` intentionally uses a
//! distinct tag.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mistlib_core::types::NodeId;

// --- Bounded `disconnect_observed_at` map ----------------------------------

/// Upper bound on `WebRtcTransport::disconnect_observed_at` entries: bounds
/// memory for peers whose disconnect was observed but that never reconnect.
/// Evicts the single oldest entry on insert once full, mirroring
/// `push_pending_candidate`'s eviction contract (`transports/webrtc.rs`).
pub(crate) const MAX_DISCONNECT_OBSERVED_ENTRIES: usize = 4096;

/// How long a `disconnect_observed_at` entry is kept before the periodic
/// sweeper drops it. Unlike `last_disconnect_at`'s short
/// `RECONNECT_COOLDOWN_MS`-scale TTL, this deliberately stays around long
/// enough to compute `downtime_ms` for a peer that takes a while to come
/// back, without holding onto every peer that disconnects and never returns
/// forever.
pub(crate) const DISCONNECT_OBSERVED_TTL_MS: u64 = 600_000;

/// Inserts `(node, at)` into `map`, evicting the single oldest entry first if
/// `map` is already at [`MAX_DISCONNECT_OBSERVED_ENTRIES`] capacity and
/// `node` isn't already a key (i.e. this insert would grow the map past the
/// cap). Pure/lock-free so the eviction behavior is unit-testable directly.
pub(crate) fn insert_disconnect_observed(
    map: &mut HashMap<NodeId, Instant>,
    node: NodeId,
    at: Instant,
) {
    if map.len() >= MAX_DISCONNECT_OBSERVED_ENTRIES && !map.contains_key(&node) {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, observed_at)| **observed_at)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(node, at);
}

// --- Rate limiting ----------------------------------------------------------

/// Max `[ConnTiming]` lines emitted per [`RATE_LIMIT_WINDOW_MS`] window,
/// across every peer/transport in this process. Protects against a
/// connect/disconnect storm (e.g. many bots reconnecting at once during a
/// load test) flooding the log.
pub(crate) const RATE_LIMIT_MAX_PER_WINDOW: u64 = 100;
pub(crate) const RATE_LIMIT_WINDOW_MS: u64 = 10_000;

struct RateLimitWindow {
    started_at: Instant,
    emitted: u64,
    suppressed: u64,
}

/// Simple windowed-counter rate limiter for `[ConnTiming]` emission: allows
/// up to `max_per_window` lines per `window`, suppressing (and counting) the
/// rest. Time is always passed in by the caller rather than read internally
/// via `Instant::now()`, so tests can drive it with fake clocks instead of
/// sleeping.
pub(crate) struct RateLimiter {
    state: Mutex<Option<RateLimitWindow>>,
    max_per_window: u64,
    window: Duration,
}

impl RateLimiter {
    pub(crate) const fn new(max_per_window: u64, window: Duration) -> Self {
        Self {
            state: Mutex::new(None),
            max_per_window,
            window,
        }
    }

    /// Call once per candidate `[ConnTiming]` event. Returns `(allow,
    /// dropped_from_expired_window)`: `allow` is whether this event should be
    /// logged; `dropped_from_expired_window` is `Some(n)` when this call
    /// happened to roll the window over and the just-expired window had `n`
    /// suppressed events -- the caller should log one `dropped=<n>` line for
    /// that expired window (in addition to, not instead of, handling `allow`
    /// for the current event).
    pub(crate) fn record(&self, now: Instant) -> (bool, Option<u64>) {
        self.roll(now, true)
    }

    /// Checks for an expired window without counting a new event, so a
    /// `dropped=<n>` summary is still emitted promptly even if no further
    /// `[ConnTiming]` event happens to follow right after the rollover --
    /// call this from a periodic tick (the session sweeper).
    pub(crate) fn poll(&self, now: Instant) -> Option<u64> {
        self.roll(now, false).1
    }

    fn roll(&self, now: Instant, record_event: bool) -> (bool, Option<u64>) {
        let mut guard = self.state.lock().unwrap();
        let expired = match &*guard {
            Some(w) => now.saturating_duration_since(w.started_at) >= self.window,
            None => true,
        };

        let mut dropped = None;
        if expired {
            if let Some(w) = guard.take() {
                if w.suppressed > 0 {
                    dropped = Some(w.suppressed);
                }
            }
            *guard = Some(RateLimitWindow {
                started_at: now,
                emitted: 0,
                suppressed: 0,
            });
        }

        if !record_event {
            return (false, dropped);
        }

        let window = guard
            .as_mut()
            .expect("window was just ensured present above");
        if window.emitted < self.max_per_window {
            window.emitted += 1;
            (true, dropped)
        } else {
            window.suppressed += 1;
            (false, dropped)
        }
    }
}

pub(crate) static RATE_LIMITER: RateLimiter = RateLimiter::new(
    RATE_LIMIT_MAX_PER_WINDOW,
    Duration::from_millis(RATE_LIMIT_WINDOW_MS),
);

// --- Log-line formatting -----------------------------------------------------
//
// Exact contract (an external log parser consumes these): `[ConnTiming] ` followed by
// space-separated `key=value` pairs in the order below. Do not deviate.

pub(crate) fn format_connect(peer: &str, attempt_ms: u64, total_connected: usize) -> String {
    format!(
        "[ConnTiming] peer={peer} kind=connect attempt_ms={attempt_ms} total_connected={total_connected}"
    )
}

pub(crate) fn format_reconnect(
    peer: &str,
    attempt_ms: u64,
    downtime_ms: u64,
    total_connected: usize,
) -> String {
    format!(
        "[ConnTiming] peer={peer} kind=reconnect attempt_ms={attempt_ms} downtime_ms={downtime_ms} total_connected={total_connected}"
    )
}

pub(crate) fn format_timeout(peer: &str, attempt_ms: u64) -> String {
    format!("[ConnTiming] peer={peer} kind=timeout attempt_ms={attempt_ms}")
}

pub(crate) fn format_dropped(dropped: u64) -> String {
    format!("[ConnTiming] dropped={dropped}")
}

/// Normalizes a disconnect `reason` for the log-line contract's
/// space-free-snake_case requirement. Every `reason` passed in today is
/// already a fixed `&'static str` literal with no spaces (`explicit_disconnect`,
/// `watchdog_connect_timeout`, `sweeper_dc_timeout`, ...) except the
/// `Failed`/`Closed` peer-connection-state path, which builds its reason
/// dynamically via `format!("peer_state_{:?}", state)` -- this keeps the
/// contract holding even if a future state's `Debug` output ever contained a
/// space, without needing every call site to remember to sanitize itself.
fn sanitize_reason(reason: &str) -> std::borrow::Cow<'_, str> {
    if reason.contains(' ') {
        std::borrow::Cow::Owned(reason.replace(' ', "_"))
    } else {
        std::borrow::Cow::Borrowed(reason)
    }
}

pub(crate) fn format_disconnect(peer: &str, reason: &str) -> String {
    format!(
        "[ConnTiming] peer={peer} kind=disconnect reason={}",
        sanitize_reason(reason)
    )
}

pub(crate) fn format_attempt_start(peer: &str) -> String {
    format!("[ConnTiming] peer={peer} kind=attempt_start")
}

// --- Emission (rate-limited) -------------------------------------------------

fn emit_dropped_if_any(dropped: Option<u64>) {
    if let Some(n) = dropped {
        tracing::info!("{}", format_dropped(n));
    }
}

/// Logs a fresh establishment (`kind=connect`), rate-limited.
pub(crate) fn log_connect(peer: &NodeId, attempt_ms: u64, total_connected: usize) {
    let (allow, dropped) = RATE_LIMITER.record(Instant::now());
    emit_dropped_if_any(dropped);
    if allow {
        tracing::info!("{}", format_connect(&peer.0, attempt_ms, total_connected));
    }
}

/// Logs a re-establishment after an observed disconnect (`kind=reconnect`),
/// rate-limited.
pub(crate) fn log_reconnect(
    peer: &NodeId,
    attempt_ms: u64,
    downtime_ms: u64,
    total_connected: usize,
) {
    let (allow, dropped) = RATE_LIMITER.record(Instant::now());
    emit_dropped_if_any(dropped);
    if allow {
        tracing::info!(
            "{}",
            format_reconnect(&peer.0, attempt_ms, downtime_ms, total_connected)
        );
    }
}

/// Logs a watchdog-forced connection-attempt timeout (`kind=timeout`),
/// rate-limited.
pub(crate) fn log_timeout(peer: &NodeId, attempt_ms: u64) {
    let (allow, dropped) = RATE_LIMITER.record(Instant::now());
    emit_dropped_if_any(dropped);
    if allow {
        tracing::info!("{}", format_timeout(&peer.0, attempt_ms));
    }
}

/// Logs a confirmed disconnect (`kind=disconnect`), rate-limited. Called
/// exactly once per disconnect from whichever teardown path resolved it
/// first -- see the call sites in `peer.rs` (`cleanup_session_impl` and the
/// `Failed`/`Closed` peer-connection-state handler) for how the two paths
/// are kept mutually exclusive for a single disconnect.
pub(crate) fn log_disconnect(peer: &NodeId, reason: &str) {
    let (allow, dropped) = RATE_LIMITER.record(Instant::now());
    emit_dropped_if_any(dropped);
    if allow {
        tracing::info!("{}", format_disconnect(&peer.0, reason));
    }
}

/// Logs the start of a connection attempt (`kind=attempt_start`), rate-limited.
/// Called from both sides of the offer/answer exchange (`connect_inner` for
/// the offering side, `handle_offer` for the answering side) at the same
/// point each already stamps `connect_started_at`.
pub(crate) fn log_attempt_start(peer: &NodeId) {
    let (allow, dropped) = RATE_LIMITER.record(Instant::now());
    emit_dropped_if_any(dropped);
    if allow {
        tracing::info!("{}", format_attempt_start(&peer.0));
    }
}

/// Periodic (non-event-driven) check for an expired rate-limit window with
/// pending suppressed events -- called from the session sweeper's tick so a
/// `dropped=<n>` summary is still emitted promptly during a quiet period
/// right after a burst, not only whenever the next `[ConnTiming]` event
/// happens to occur.
pub(crate) fn poll_dropped_summary() {
    emit_dropped_if_any(RATE_LIMITER.poll(Instant::now()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_connect_matches_exact_contract() {
        assert_eq!(
            format_connect("peer-1", 123, 4),
            "[ConnTiming] peer=peer-1 kind=connect attempt_ms=123 total_connected=4"
        );
    }

    #[test]
    fn format_reconnect_matches_exact_contract() {
        assert_eq!(
            format_reconnect("peer-1", 123, 456, 4),
            "[ConnTiming] peer=peer-1 kind=reconnect attempt_ms=123 downtime_ms=456 total_connected=4"
        );
    }

    #[test]
    fn format_timeout_matches_exact_contract() {
        assert_eq!(
            format_timeout("peer-1", 6000),
            "[ConnTiming] peer=peer-1 kind=timeout attempt_ms=6000"
        );
    }

    #[test]
    fn format_dropped_matches_exact_contract() {
        assert_eq!(format_dropped(7), "[ConnTiming] dropped=7");
    }

    #[test]
    fn format_disconnect_matches_exact_contract() {
        assert_eq!(
            format_disconnect("peer-1", "explicit_disconnect"),
            "[ConnTiming] peer=peer-1 kind=disconnect reason=explicit_disconnect"
        );
    }

    #[test]
    fn format_disconnect_sanitizes_spaces_in_reason() {
        // Every real call site already passes a space-free snake_case
        // reason, but the format must hold the contract even if a future
        // dynamically-built reason (e.g. from a Debug impl) ever contained
        // one.
        assert_eq!(
            format_disconnect("peer-1", "peer state Failed"),
            "[ConnTiming] peer=peer-1 kind=disconnect reason=peer_state_Failed"
        );
    }

    #[test]
    fn format_attempt_start_matches_exact_contract() {
        assert_eq!(
            format_attempt_start("peer-1"),
            "[ConnTiming] peer=peer-1 kind=attempt_start"
        );
    }

    #[test]
    fn no_formatted_line_contains_the_cs_tag() {
        // The log parser filters any line containing "[CS]" -- none of
        // these formats may ever produce that substring.
        for line in [
            format_connect("peer-1", 1, 1),
            format_reconnect("peer-1", 1, 1, 1),
            format_timeout("peer-1", 1),
            format_dropped(1),
            format_disconnect("peer-1", "explicit_disconnect"),
            format_attempt_start("peer-1"),
        ] {
            assert!(!line.contains("[CS]"), "line must not contain [CS]: {line}");
        }
    }

    #[test]
    fn rate_limiter_allows_up_to_the_cap_within_a_window() {
        let limiter = RateLimiter::new(3, Duration::from_millis(10_000));
        let base = Instant::now();
        for _ in 0..3 {
            let (allow, dropped) = limiter.record(base);
            assert!(allow);
            assert_eq!(dropped, None);
        }
    }

    #[test]
    fn rate_limiter_suppresses_past_the_cap_within_a_window() {
        let limiter = RateLimiter::new(2, Duration::from_millis(10_000));
        let base = Instant::now();
        assert!(limiter.record(base).0);
        assert!(limiter.record(base).0);
        let (allow, dropped) = limiter.record(base);
        assert!(
            !allow,
            "third event within the same window must be suppressed"
        );
        assert_eq!(dropped, None, "no window has expired yet");
    }

    #[test]
    fn rate_limiter_reports_dropped_count_on_next_window_rollover() {
        let limiter = RateLimiter::new(1, Duration::from_millis(10));
        let base = Instant::now();
        assert!(limiter.record(base).0);
        // Both of these are suppressed: over the cap within the same window.
        assert!(!limiter.record(base).0);
        assert!(!limiter.record(base).0);

        // Advance past the window boundary: this call rolls the window over
        // and must report the two suppressed events from the expired window.
        let next_window = base + Duration::from_millis(11);
        let (allow, dropped) = limiter.record(next_window);
        assert!(allow, "the first event in a fresh window must be allowed");
        assert_eq!(dropped, Some(2));
    }

    #[test]
    fn rate_limiter_does_not_report_dropped_when_nothing_was_suppressed() {
        let limiter = RateLimiter::new(5, Duration::from_millis(10));
        let base = Instant::now();
        assert!(limiter.record(base).0);
        let next_window = base + Duration::from_millis(11);
        let (allow, dropped) = limiter.record(next_window);
        assert!(allow);
        assert_eq!(dropped, None);
    }

    #[test]
    fn poll_reports_dropped_without_counting_a_new_event() {
        let limiter = RateLimiter::new(1, Duration::from_millis(10));
        let base = Instant::now();
        assert!(limiter.record(base).0);
        assert!(!limiter.record(base).0); // suppressed

        let next_window = base + Duration::from_millis(11);
        let dropped = limiter.poll(next_window);
        assert_eq!(dropped, Some(1));

        // The window `poll` just rolled into is otherwise empty -- confirm a
        // subsequent `record` in that same (now-current) window still gets
        // the full fresh allowance instead of being treated as still over
        // the old cap.
        assert!(limiter.record(next_window).0);
    }

    #[test]
    fn insert_disconnect_observed_does_not_evict_under_the_cap() {
        let mut map = HashMap::new();
        let now = Instant::now();
        for i in 0..MAX_DISCONNECT_OBSERVED_ENTRIES {
            insert_disconnect_observed(&mut map, NodeId(format!("peer-{i}")), now);
        }
        assert_eq!(map.len(), MAX_DISCONNECT_OBSERVED_ENTRIES);
    }

    #[test]
    fn insert_disconnect_observed_evicts_oldest_past_the_cap() {
        let mut map = HashMap::new();
        let base = Instant::now();
        for i in 0..MAX_DISCONNECT_OBSERVED_ENTRIES {
            insert_disconnect_observed(
                &mut map,
                NodeId(format!("peer-{i}")),
                base + Duration::from_millis(i as u64),
            );
        }

        insert_disconnect_observed(
            &mut map,
            NodeId("peer-overflow".to_string()),
            base + Duration::from_millis(MAX_DISCONNECT_OBSERVED_ENTRIES as u64),
        );

        assert_eq!(map.len(), MAX_DISCONNECT_OBSERVED_ENTRIES);
        assert!(
            !map.contains_key(&NodeId("peer-0".to_string())),
            "the oldest entry must be evicted"
        );
        assert!(map.contains_key(&NodeId("peer-overflow".to_string())));
    }

    #[test]
    fn insert_disconnect_observed_overwriting_an_existing_key_does_not_evict() {
        let mut map = HashMap::new();
        let now = Instant::now();
        for i in 0..MAX_DISCONNECT_OBSERVED_ENTRIES {
            insert_disconnect_observed(&mut map, NodeId(format!("peer-{i}")), now);
        }
        let later = now + Duration::from_secs(1);
        insert_disconnect_observed(&mut map, NodeId("peer-0".to_string()), later);
        assert_eq!(
            map.len(),
            MAX_DISCONNECT_OBSERVED_ENTRIES,
            "re-inserting an existing key must not change the map size"
        );
        assert_eq!(map.get(&NodeId("peer-0".to_string())), Some(&later));
    }
}
