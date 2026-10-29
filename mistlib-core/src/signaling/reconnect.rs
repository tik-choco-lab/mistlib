use rand::Rng;
use web_time::{Duration, Instant};

const INITIAL_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 30_000;
const JITTER_FRACTION: f64 = 0.20;

/// A connection must stay up at least this long before a later disconnect
/// is treated as a fresh failure sequence (backoff reset).
pub const STABLE_CONNECTION_RESET: Duration = Duration::from_secs(30);

/// Per-relay reconnect backoff that only resets after a connection proved stable.
#[derive(Debug, Clone, Default)]
pub struct ReconnectBackoff {
    attempt: u32,
    connected_at: Option<Instant>,
}

impl ReconnectBackoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the next delay with random +/-20% jitter and advances the attempt counter.
    pub fn next_delay(&mut self) -> Duration {
        let jitter = rand::thread_rng().gen_range(-JITTER_FRACTION..=JITTER_FRACTION);
        self.next_delay_with_jitter(jitter)
    }

    /// Returns the next delay with explicit jitter and advances the attempt counter.
    pub fn next_delay_with_jitter(&mut self, jitter: f64) -> Duration {
        let delay = reconnect_backoff_delay(self.attempt, jitter);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// Call when a connection opened.
    pub fn on_connected(&mut self, now: Instant) {
        self.connected_at = Some(now);
    }

    /// Resets the attempt counter only if the connection stayed open long enough.
    pub fn on_disconnected(&mut self, now: Instant) {
        if let Some(connected_at) = self.connected_at.take() {
            if now.checked_duration_since(connected_at) >= Some(STABLE_CONNECTION_RESET) {
                self.attempt = 0;
            }
        }
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

/// Returns the reconnect delay for a zero-based failed-attempt index.
///
/// `jitter` is clamped to the required +/-20% range so tests can exercise the
/// pure calculation without depending on randomness.
pub fn reconnect_backoff_delay(attempt: u32, jitter: f64) -> Duration {
    let exp = attempt.min(16);
    let base = INITIAL_BACKOFF_MS
        .saturating_mul(1_u64 << exp)
        .min(MAX_BACKOFF_MS);
    let jitter = jitter.clamp(-JITTER_FRACTION, JITTER_FRACTION);
    let millis = (base as f64 * (1.0 + jitter)).round() as u64;
    Duration::from_millis(millis.max(1))
}

pub fn random_reconnect_backoff_delay(attempt: u32) -> Duration {
    let jitter = rand::thread_rng().gen_range(-JITTER_FRACTION..=JITTER_FRACTION);
    reconnect_backoff_delay(attempt, jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_until_cap_without_jitter() {
        let delays: Vec<_> = (0..8)
            .map(|attempt| reconnect_backoff_delay(attempt, 0.0).as_millis())
            .collect();
        assert_eq!(
            delays,
            vec![500, 1000, 2000, 4000, 8000, 16000, 30000, 30000]
        );
    }

    #[test]
    fn backoff_clamps_jitter_to_twenty_percent() {
        assert_eq!(reconnect_backoff_delay(0, -1.0).as_millis(), 400);
        assert_eq!(reconnect_backoff_delay(0, 1.0).as_millis(), 600);
        assert_eq!(reconnect_backoff_delay(10, 1.0).as_millis(), 36000);
    }

    #[test]
    fn reconnect_backoff_grows_through_short_connections() {
        let mut backoff = ReconnectBackoff::new();
        let now = Instant::now();
        for expected in [500, 1000, 2000, 4000] {
            assert_eq!(backoff.next_delay_with_jitter(0.0).as_millis(), expected);
            backoff.on_connected(now);
            backoff.on_disconnected(now + Duration::from_secs(1));
        }
        assert_eq!(backoff.attempt(), 4);
    }

    #[test]
    fn reconnect_backoff_resets_after_stable_connection() {
        let mut backoff = ReconnectBackoff::new();
        let now = Instant::now();
        backoff.next_delay_with_jitter(0.0);
        backoff.on_connected(now);
        backoff.on_disconnected(now + STABLE_CONNECTION_RESET);
        assert_eq!(backoff.attempt(), 0);
        assert_eq!(backoff.next_delay_with_jitter(0.0).as_millis(), 500);
    }

    #[test]
    fn reconnect_disconnected_without_connection_does_not_reset() {
        let mut backoff = ReconnectBackoff::new();
        backoff.next_delay_with_jitter(0.0);
        backoff.on_disconnected(Instant::now());
        assert_eq!(backoff.attempt(), 1);
        assert_eq!(backoff.next_delay_with_jitter(0.0).as_millis(), 1000);
    }
}
