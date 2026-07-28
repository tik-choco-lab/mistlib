use rand::Rng;
use web_time::Duration;

const INITIAL_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 30_000;
const JITTER_FRACTION: f64 = 0.20;

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
}
