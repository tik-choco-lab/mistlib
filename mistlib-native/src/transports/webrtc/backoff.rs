//! Exponential backoff schedule shared by every bounded retry loop in the
//! WebRTC transport that used to sleep a fixed interval between attempts:
//! `signaling::spawn_connect_request_retry` (the higher-ID side's
//! `CONNECT_REQUEST` nudge to the lower-ID/initiator peer) and
//! `peer::PeerSharedHandles::try_ice_restart` (the ICE-restart retry).
//!
//! A fixed short interval repeated many times turns a burst of simultaneous
//! reconnects (e.g. a "cluster disconnect" where several peers drop within
//! the same couple of seconds) into a synchronized retry storm that keeps
//! re-hitting the same `handshake_semaphore` slots and the same remote
//! peers at the same cadence. Backing off exponentially (capped, so it never
//! grows unbounded) spreads those retries out over time instead, while
//! still keeping the *total* time budget for "give up and let a fresh
//! `connect()`/DNVE3 reselection start over" in the same rough ballpark as
//! before.
//!
//! Kept as a pure function (no timer, no transport/lock state) so the
//! schedule itself is exhaustively unit-testable without ever waiting on a
//! real `tokio::time::sleep`.

/// Delay before the `attempt_number`-th retry, under an exponential backoff
/// schedule: `initial_ms * multiplier^(attempt_number - 1)`, capped at
/// `max_ms`. `attempt_number` is 1-indexed -- `attempt_number == 1` (the
/// delay before the *first* retry) always returns `initial_ms` unchanged
/// (mirrors the old fixed-interval behavior for the first retry).
/// `attempt_number == 0` returns `0` (nothing to wait for).
///
/// Defensive against a degenerate `initial_ms > max_ms` configuration: the
/// very first computed delay is clamped to `max_ms` too, same as every
/// later one, rather than overshooting once before the cap kicks in.
pub(crate) fn exponential_backoff_ms(
    attempt_number: u32,
    initial_ms: u64,
    multiplier: f64,
    max_ms: u64,
) -> u64 {
    if attempt_number == 0 {
        return 0;
    }
    let exponent = (attempt_number - 1) as i32;
    let scaled = initial_ms as f64 * multiplier.powi(exponent);
    if !scaled.is_finite() || scaled >= max_ms as f64 {
        max_ms
    } else {
        scaled.round() as u64
    }
}

/// Sum of `exponential_backoff_ms(1..=attempt_count, ...)` -- the total wall
/// time a retry loop that sleeps once per attempt (before attempts
/// `2..=attempt_count + 1`) actually spends waiting. Used by tests (and
/// available to callers, e.g. for logging/diagnostics) that need to reason
/// about the *total* retry budget rather than any single interval.
#[cfg(test)]
pub(crate) fn total_backoff_ms(
    attempt_count: u32,
    initial_ms: u64,
    multiplier: f64,
    max_ms: u64,
) -> u64 {
    (1..=attempt_count)
        .map(|attempt| exponential_backoff_ms(attempt, initial_ms, multiplier, max_ms))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_attempt_returns_the_initial_interval_unchanged() {
        assert_eq!(exponential_backoff_ms(1, 1000, 1.5, 10_000), 1000);
    }

    #[test]
    fn zeroth_attempt_needs_no_wait() {
        assert_eq!(exponential_backoff_ms(0, 1000, 1.5, 10_000), 0);
    }

    #[test]
    fn grows_by_the_multiplier_each_attempt_until_the_cap() {
        // 1000, 1500, 2250, 3375, 5062.5 -> 5063 (rounded), 7593.75 -> 7594
        assert_eq!(exponential_backoff_ms(1, 1000, 1.5, 10_000), 1000);
        assert_eq!(exponential_backoff_ms(2, 1000, 1.5, 10_000), 1500);
        assert_eq!(exponential_backoff_ms(3, 1000, 1.5, 10_000), 2250);
        assert_eq!(exponential_backoff_ms(4, 1000, 1.5, 10_000), 3375);
        assert_eq!(exponential_backoff_ms(5, 1000, 1.5, 10_000), 5063);
        assert_eq!(exponential_backoff_ms(6, 1000, 1.5, 10_000), 7594);
    }

    #[test]
    fn caps_at_max_ms_once_the_schedule_would_exceed_it() {
        assert_eq!(exponential_backoff_ms(7, 1000, 1.5, 10_000), 10_000);
        assert_eq!(exponential_backoff_ms(20, 1000, 1.5, 10_000), 10_000);
        assert_eq!(exponential_backoff_ms(1_000_000, 1000, 1.5, 10_000), 10_000);
    }

    #[test]
    fn clamps_a_degenerate_initial_greater_than_max_from_the_first_attempt() {
        assert_eq!(exponential_backoff_ms(1, 50_000, 2.0, 10_000), 10_000);
    }

    #[test]
    fn connect_request_default_schedule_totals_within_the_intended_ballpark() {
        // Mirrors `WebRtcTransport`'s CONNECT_REQUEST_RETRY_* constants and
        // `DEFAULT_CONNECT_REQUEST_RETRIES` (10 sends == 9 waited intervals).
        //
        // The cap was originally 10s (total ~50.8s), then lowered to 4s
        // (total ~28.1s) after a load-test A/B showed the 10s cap improved
        // the typical-case recovery time but *worsened* the worst case (max
        // recovery 198s -> 316s) and the unrelated first-connect `attempt_ms`
        // tail, while the timeout count/retry-chain-length distribution were
        // unchanged between the two runs -- i.e. the longer cap bought no
        // extra success probability, only a longer tail. See the doc comment
        // on `CONNECT_REQUEST_RETRY_MAX_INTERVAL_MS` (`webrtc.rs`) for the
        // full writeup.
        let total = total_backoff_ms(9, 1000, 1.5, 4_000);
        assert_eq!(total, 28_125, "exact schedule: 1000+1500+2250+3375+4000*5");
        assert!(
            (25_000..=35_000).contains(&total),
            "expected total backoff within [25s, 35s] (~30s ballpark), got {total}ms"
        );
    }

    #[test]
    fn ice_restart_default_schedule_stays_well_inside_the_disconnect_grace_window() {
        // Mirrors `peer::ICE_RESTART_RETRY_*` production constants (2
        // waited intervals for 3 attempts): must stay well under the 5s
        // `DISCONNECTED_GRACE_MS` default so the sweeper never reaps a
        // session that ICE restart is still actively working on.
        let total = total_backoff_ms(2, 500, 2.0, 2_000);
        assert!(
            total < 5_000,
            "ICE restart backoff must stay well under the 5s disconnect grace window, got {total}ms"
        );
    }
}
