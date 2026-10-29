use rand::Rng;
use web_time::Duration;

/// What to do after a relay CLOSED one of our subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedAction {
    /// Re-issue the REQ for this subscription after the given delay.
    ResubscribeAfter(Duration),
    /// Stop re-subscribing this subscription on this connection.
    GiveUp,
}

/// Returns whether retrying a CLOSED reason on the same connection cannot help.
pub fn is_permanent_closed_reason(message: &str) -> bool {
    let prefix = message.split(':').next().unwrap_or_default().trim();
    [
        "auth-required",
        "restricted",
        "blocked",
        "invalid",
        "unsupported",
        "pow",
        "mute",
    ]
    .iter()
    .any(|permanent| prefix.eq_ignore_ascii_case(permanent))
}

pub const CLOSED_RESUBSCRIBE_MAX_ATTEMPTS: u32 = 5;
pub const CLOSED_RESUBSCRIBE_INITIAL: Duration = Duration::from_secs(1);
pub const CLOSED_RESUBSCRIBE_MAX: Duration = Duration::from_secs(60);

const JITTER_FRACTION: f64 = 0.20;

/// Per-connection, per-subscription CLOSED budget.
#[derive(Debug, Clone, Default)]
pub struct ClosedResubscribe {
    consecutive: u32,
}

impl ClosedResubscribe {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handles a CLOSED reason with random +/-20% jitter.
    pub fn on_closed(&mut self, message: &str) -> ClosedAction {
        let jitter = rand::thread_rng().gen_range(-JITTER_FRACTION..=JITTER_FRACTION);
        self.on_closed_with_jitter(message, jitter)
    }

    /// Handles a CLOSED reason with explicit jitter.
    pub fn on_closed_with_jitter(&mut self, message: &str, jitter: f64) -> ClosedAction {
        if is_permanent_closed_reason(message) {
            return ClosedAction::GiveUp;
        }
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive > CLOSED_RESUBSCRIBE_MAX_ATTEMPTS {
            return ClosedAction::GiveUp;
        }

        let base = (CLOSED_RESUBSCRIBE_INITIAL * (1 << (self.consecutive - 1)))
            .min(CLOSED_RESUBSCRIBE_MAX);
        let jitter = jitter.clamp(-JITTER_FRACTION, JITTER_FRACTION);
        let millis = (base.as_millis() as f64 * (1.0 + jitter)).round() as u64;
        ClosedAction::ResubscribeAfter(Duration::from_millis(millis))
    }

    /// Call when the relay accepted the subscription (EOSE or EVENT received for it).
    pub fn on_accepted(&mut self) {
        self.consecutive = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_permanent_reasons_give_up() {
        for prefix in [
            "auth-required",
            "restricted",
            "blocked",
            "invalid",
            "unsupported",
            "pow",
            "mute",
        ] {
            assert_eq!(
                ClosedResubscribe::new().on_closed_with_jitter(&format!(" {prefix}: detail"), 0.0),
                ClosedAction::GiveUp
            );
            assert_eq!(
                ClosedResubscribe::new().on_closed_with_jitter(&prefix.to_ascii_uppercase(), 0.0),
                ClosedAction::GiveUp
            );
        }
    }

    #[test]
    fn closed_retry_budget_and_acceptance() {
        let mut budget = ClosedResubscribe::new();
        for seconds in [1, 2, 4, 8, 16] {
            assert_eq!(
                budget.on_closed_with_jitter("rate-limited: slow down", 0.0),
                ClosedAction::ResubscribeAfter(Duration::from_secs(seconds))
            );
        }
        assert_eq!(
            budget.on_closed_with_jitter("rate-limited: slow down", 0.0),
            ClosedAction::GiveUp
        );
        budget.on_accepted();
        assert_eq!(
            budget.on_closed_with_jitter("", 0.0),
            ClosedAction::ResubscribeAfter(Duration::from_secs(1))
        );
    }

    #[test]
    fn closed_retryable_reasons_and_jitter() {
        for reason in ["", "error: failure", "duplicate: id", "unknown: reason"] {
            assert!(!is_permanent_closed_reason(reason));
        }
        assert_eq!(
            ClosedResubscribe::new().on_closed_with_jitter("", -1.0),
            ClosedAction::ResubscribeAfter(Duration::from_millis(800))
        );
        assert_eq!(
            ClosedResubscribe::new().on_closed_with_jitter("", 1.0),
            ClosedAction::ResubscribeAfter(Duration::from_millis(1200))
        );
    }
}
