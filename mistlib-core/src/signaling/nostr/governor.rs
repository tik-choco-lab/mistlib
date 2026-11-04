//! Relay publication policy (SPEC-23 B2). No I/O, timers, or platform tasks.
//! Defaults derive from Phase A U1/U6: 16 EVENT in the busiest healthy second,
//! while negotiation accounted for most rejected events in S1/S3.
use super::event::RelayMessage;
use std::collections::{HashMap, VecDeque};
use web_time::{Duration, Instant};

/// Leaves room above the measured healthy six-node peak (16 EVENT/s).
pub const BUCKET_BURST: f64 = 20.0;
/// Sustainable autonomous ceiling; healthy six-node average was 0.67 EVENT/s.
pub const BUCKET_RATE: f64 = 4.0;
pub const BUCKET_MIN_RATE: f64 = 0.5;
pub const RATE_RECOVERY_STEP: f64 = 0.5;
pub const QUIET_PERIOD: Duration = Duration::from_secs(10);
pub const COOLDOWN_INITIAL: Duration = Duration::from_secs(2);
pub const COOLDOWN_MAX: Duration = Duration::from_secs(60);
/// At most three additional attempts, preserving the signed event and id.
pub const NEGOTIATION_RETRY: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// Three distinct cooldown episodes on every open relay suspend proactive work.
/// An in-flight rejection burst counts as one episode.
pub const SUSPEND_REJECTIONS: u32 = 3;
/// Low-priority traffic is disposable; negotiation has no eviction path.
pub const LOW_QUEUE_LIMIT: usize = 256;
const HISTORY_TTL: Duration = Duration::from_secs(120);
const HISTORY_LIMIT: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PublishPriority {
    Negotiation,
    Reply,
    Probe,
    Ordinary,
}
impl PublishPriority {
    pub fn from_label(label: &str) -> Self {
        match label {
            "offer" | "answer" | "candidate" | "candidates" | "candidate_ack"
            | "negotiation_ack" | "request" => Self::Negotiation,
            "request_reply" => Self::Reply,
            "request_probe" => Self::Probe,
            _ => Self::Ordinary,
        }
    }
}
#[derive(Clone, Debug)]
struct Publication {
    id: String,
    frame: String,
    priority: PublishPriority,
    target: String,
    generation: u64,
    attempt: u64,
    expires: Instant,
}
#[derive(Debug)]
struct Pending {
    publication: Publication,
    due: Instant,
    retries: usize,
}
#[derive(Debug)]
struct Relay {
    active: bool,
    tokens: f64,
    rate: f64,
    updated: Instant,
    quiet_since: Instant,
    cooldown: Option<Instant>,
    cooldowns: u32,
    queue: VecDeque<Pending>,
    last_target: [Option<String>; 4],
}
impl Relay {
    fn new(now: Instant) -> Self {
        Self {
            active: true,
            tokens: BUCKET_BURST,
            rate: BUCKET_RATE,
            updated: now,
            quiet_since: now,
            cooldown: None,
            cooldowns: 0,
            queue: VecDeque::new(),
            last_target: Default::default(),
        }
    }
    fn advance(&mut self, now: Instant, name: &str) {
        self.tokens = (self.tokens
            + now.saturating_duration_since(self.updated).as_secs_f64() * self.rate)
            .min(BUCKET_BURST);
        self.updated = now;
        if self.cooldown.is_some_and(|until| now >= until) {
            self.cooldown = None;
            tracing::info!("[NostrGovernor] action=cooldown_exit relay={name}");
        }
        if now.saturating_duration_since(self.quiet_since) >= QUIET_PERIOD {
            let steps = (now
                .saturating_duration_since(self.quiet_since)
                .as_secs_f64()
                / QUIET_PERIOD.as_secs_f64())
            .floor();
            self.rate = (self.rate + RATE_RECOVERY_STEP * steps).min(BUCKET_RATE);
            self.quiet_since += QUIET_PERIOD.mul_f64(steps);
            self.cooldowns = 0;
            tracing::debug!(
                "[NostrGovernor] action=recover relay={name} rate={}",
                self.rate
            );
        }
    }
}

#[derive(Debug, Default)]
pub struct PublishGovernor {
    relays: HashMap<String, Relay>,
    history: HashMap<String, Publication>,
    accepted: HashMap<String, Instant>,
    attempts: HashMap<String, u64>,
    generation: u64,
    suspended: bool,
    peer_pubkeys: HashMap<String, String>,
    retry_counts: HashMap<(String, String), usize>,
}
impl PublishGovernor {
    pub fn register(&mut self, id: &str, frame: String, label: &str, target: &str, now: Instant) {
        self.history.retain(|_, p| p.expires > now);
        self.retry_counts
            .retain(|(_, id), _| self.history.contains_key(id));
        if self.history.len() >= HISTORY_LIMIT {
            if let Some(old) = self
                .history
                .iter()
                .min_by_key(|(_, p)| p.expires)
                .map(|(id, _)| id.clone())
            {
                self.history.remove(&old);
            }
        }
        self.history.insert(
            id.into(),
            Publication {
                id: id.into(),
                frame,
                priority: PublishPriority::from_label(label),
                target: target.into(),
                generation: self.generation,
                attempt: *self.attempts.get(target).unwrap_or(&0),
                expires: now + HISTORY_TTL,
            },
        );
    }
    pub fn open(&mut self, relay: &str, now: Instant) {
        self.relays
            .entry(relay.into())
            .or_insert_with(|| Relay::new(now))
            .active = true;
    }
    pub fn disconnected(&mut self, relay: &str) {
        if let Some(state) = self.relays.get_mut(relay) {
            state.active = false;
        }
    }
    pub fn reset_session(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        // Keep registrations until expiry so frames still in adapter channels
        // retain their old generation and cannot masquerade as new discovery.
        self.accepted.clear();
        self.attempts.clear();
        self.retry_counts.clear();
        self.peer_pubkeys.clear();
        for relay in self.relays.values_mut() {
            relay.queue.clear();
        }
        // Relay rate limits deliberately survive identity/session changes.
    }
    pub fn bind_peer(&mut self, node: &str, pubkey: &str) {
        self.peer_pubkeys.insert(node.into(), pubkey.into());
    }
    pub fn end_peer_attempt(&mut self, node: &str) {
        if let Some(pubkey) = self.peer_pubkeys.get(node).cloned() {
            self.end_attempt(&pubkey);
        }
    }
    pub fn end_attempt(&mut self, target: &str) {
        let attempt = self.attempts.entry(target.into()).or_default();
        *attempt = attempt.wrapping_add(1);
    }
    pub fn retire(&mut self, pubkey: &str) {
        for relay in self.relays.values_mut() {
            relay.queue.retain(|p| p.publication.target != pubkey);
        }
        self.end_attempt(pubkey);
    }
    pub fn suspended(&mut self, now: Instant) -> bool {
        self.advance(now);
        self.suspended
    }
    fn advance(&mut self, now: Instant) {
        self.accepted.retain(|_, until| *until > now);
        for (name, relay) in &mut self.relays {
            relay.advance(now, name);
        }
        let suspended = self.relays.values().any(|r| r.active)
            && self
                .relays
                .values()
                .filter(|r| r.active)
                .all(|r| r.cooldowns >= SUSPEND_REJECTIONS);
        if suspended != self.suspended {
            self.suspended = suspended;
            tracing::info!(
                "[NostrGovernor] action={} reason=all_relays",
                if suspended {
                    "suspend_enter"
                } else {
                    "suspend_exit"
                }
            );
        }
    }
    /// Returns control frames immediately; EVENTs enter a priority queue.
    pub fn enqueue(&mut self, relay: &str, frame: String, now: Instant) -> Option<String> {
        self.open(relay, now);
        let id = event_id(&frame);
        let Some(id) = id else {
            return Some(frame);
        };
        let publication = self
            .history
            .get(&id)
            .cloned()
            .unwrap_or_else(|| Publication {
                id,
                frame: frame.clone(),
                priority: PublishPriority::Ordinary,
                target: String::new(),
                generation: self.generation,
                attempt: 0,
                expires: now + HISTORY_TTL,
            });
        let queue = &mut self.relays.get_mut(relay).unwrap().queue;
        // Coalesce refreshes and repeated discovery requests before dropping.
        if publication.priority != PublishPriority::Negotiation {
            queue.retain(|p| {
                p.publication.priority != publication.priority
                    || p.publication.target != publication.target
            });
            if queue.len() >= LOW_QUEUE_LIMIT {
                tracing::debug!("[NostrGovernor] action=drop reason=low_queue relay={relay}");
                return None;
            }
        }
        queue.push_back(Pending {
            publication,
            due: now,
            retries: 0,
        });
        None
    }
    pub fn poll(&mut self, relay: &str, now: Instant) -> Option<String> {
        self.advance(now);
        let state = self.relays.get_mut(relay)?;
        state.queue.retain(|p| {
            let current = p.publication.generation == self.generation
                && p.publication.attempt == *self.attempts.get(&p.publication.target).unwrap_or(&0);
            let useful = p.retries == 0 || !self.accepted.contains_key(&p.publication.id);
            if !current || !useful {
                tracing::debug!(
                    "[NostrGovernor] action=drop reason=stale_or_accepted event_id={}",
                    p.publication.id
                );
            }
            current && useful
        });
        if state.tokens < 1.0 {
            return None;
        }
        let index = state
            .queue
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.due <= now
                    && (p.publication.priority == PublishPriority::Negotiation
                        || (!self.suspended && state.cooldown.is_none()))
            })
            .min_by_key(|(_, p)| {
                let publication = &p.publication;
                let last = &state.last_target[publication.priority as usize];
                (
                    publication.priority,
                    last.as_ref().is_some_and(|t| publication.target <= *t),
                    &publication.target,
                )
            })
            .map(|(i, _)| i)?;
        let pending = state.queue.remove(index)?;
        state.last_target[pending.publication.priority as usize] =
            Some(pending.publication.target.clone());
        state.tokens -= 1.0;
        if pending.retries > 0 {
            tracing::info!(
                "[NostrGovernor] action=negotiation_retry relay={relay} event_id={} attempt={}",
                pending.publication.id,
                pending.retries
            );
        }
        // Keep sent frames for relay OK; queued work itself never expires.
        self.history
            .entry(pending.publication.id.clone())
            .or_insert_with(|| pending.publication.clone());
        Some(pending.publication.frame)
    }
    pub fn has_pending(&self) -> bool {
        self.relays.values().any(|r| !r.queue.is_empty())
    }
    pub fn next_wakeup(&mut self, relay: &str, now: Instant) -> Option<Duration> {
        self.advance(now);
        let state = self.relays.get(relay)?;
        let tokens_at = if state.tokens >= 1.0 {
            now
        } else {
            now + Duration::from_secs_f64((1.0 - state.tokens) / state.rate)
        };
        state
            .queue
            .iter()
            .map(|p| {
                let mut at = p.due.max(tokens_at);
                if p.publication.priority != PublishPriority::Negotiation {
                    if let Some(until) = state.cooldown {
                        at = at.max(until);
                    }
                    if self.suspended {
                        at = at.max(state.quiet_since + QUIET_PERIOD);
                    }
                }
                at.saturating_duration_since(now)
            })
            .min()
    }

    pub fn observe(&mut self, relay: &str, message: &RelayMessage, now: Instant) {
        match message {
            RelayMessage::Ok {
                event_id,
                accepted: true,
                ..
            } => {
                self.accepted.insert(event_id.clone(), now + HISTORY_TTL);
            }
            RelayMessage::Ok {
                event_id,
                accepted: false,
                message,
            } if rate_limited(message) => {
                self.rate_limit(relay, now);
                if self.accepted.contains_key(event_id) {
                    return;
                }
                if let Some(publication) = self
                    .history
                    .get(event_id)
                    .filter(|p| p.priority == PublishPriority::Negotiation)
                    .cloned()
                {
                    let state = self.relays.get_mut(relay).unwrap();
                    if state
                        .queue
                        .iter()
                        .any(|p| p.publication.id == *event_id && p.retries > 0)
                    {
                        return;
                    }
                    // Retry count is per event and relay, including frames in flight.
                    let retries = self
                        .retry_counts
                        .entry((relay.into(), event_id.clone()))
                        .or_default();
                    if (*retries) < NEGOTIATION_RETRY.len() {
                        let due = now + NEGOTIATION_RETRY[*retries];
                        *retries += 1;
                        state.queue.push_back(Pending {
                            publication,
                            due,
                            retries: *retries,
                        });
                    }
                }
            }
            RelayMessage::Ok {
                event_id,
                accepted: false,
                ..
            } => {
                if let Some(state) = self.relays.get_mut(relay) {
                    state
                        .queue
                        .retain(|p| p.publication.id != *event_id || p.retries == 0);
                }
            }
            RelayMessage::Closed { message, .. } | RelayMessage::Notice(message)
                if rate_limited(message) =>
            {
                self.rate_limit(relay, now)
            }
            _ => {}
        }
    }
    fn rate_limit(&mut self, relay: &str, now: Instant) {
        self.open(relay, now);
        let state = self.relays.get_mut(relay).unwrap();
        state.advance(now, relay);
        state.quiet_since = now;
        // A relay may reject a whole in-flight burst in one RTT. Treat that
        // burst as one congestion episode, not one multiplicative decrease
        // or suspension vote per frame. Retry scheduling still runs per event.
        if state.cooldown.is_some_and(|until| until > now) {
            return;
        }
        state.rate = (state.rate / 2.0).max(BUCKET_MIN_RATE);
        state.cooldowns = state.cooldowns.saturating_add(1);
        let seconds = (COOLDOWN_INITIAL
            .as_secs()
            .saturating_mul(1 << state.cooldowns.saturating_sub(1).min(5)))
        .min(COOLDOWN_MAX.as_secs());
        state.cooldown = Some(now + Duration::from_secs(seconds));
        state.quiet_since = now;
        // Do not discharge the initial burst again after rejection.
        state.tokens = state.tokens.min(1.0);
        tracing::warn!(
            "[NostrGovernor] action=cooldown_enter relay={relay} delay_ms={} rate={}",
            seconds * 1000,
            state.rate
        );
        self.advance(now);
    }
}
pub fn rate_limited(message: &str) -> bool {
    message
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("rate-limited:")
}
fn event_id(frame: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(frame).ok()?;
    (value[0] == "EVENT")
        .then(|| value[1]["id"].as_str().map(str::to_owned))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(id: &str) -> String {
        format!(r#"["EVENT",{{"id":"{id}"}}]"#)
    }
    fn add(g: &mut PublishGovernor, id: &str, label: &str, now: Instant) {
        g.register(id, frame(id), label, id, now);
        g.enqueue("r", frame(id), now);
    }
    fn reject(id: &str) -> RelayMessage {
        RelayMessage::Ok {
            event_id: id.into(),
            accepted: false,
            message: "rate-limited: slow".into(),
        }
    }
    #[test]
    fn healthy_burst_and_priority_then_refill() {
        let now = Instant::now();
        let mut g = PublishGovernor::default();
        add(&mut g, "low", "discovery", now);
        add(&mut g, "high", "answer", now);
        assert_eq!(g.poll("r", now), Some(frame("high")));
        assert_eq!(g.poll("r", now), Some(frame("low")));
        for i in 0..20 {
            add(&mut g, &i.to_string(), "candidate", now);
        }
        for _ in 0..18 {
            assert!(g.poll("r", now).is_some());
        }
        assert!(g.poll("r", now).is_none());
        assert!(g.poll("r", now + Duration::from_millis(250)).is_some());
    }
    #[test]
    fn rejection_burst_is_one_cooldown_and_recovers_from_two_per_second() {
        let now = Instant::now();
        let mut g = PublishGovernor::default();
        for _ in 0..20 {
            g.observe("r", &reject("x"), now);
        }
        assert!(!g.suspended(now));
        assert_eq!(g.relays["r"].rate, 2.0);
        assert_eq!(g.relays["r"].cooldowns, 1);
        g.suspended(now + Duration::from_secs(40));
        assert_eq!(g.relays["r"].rate, 4.0);
    }
    #[test]
    fn three_cooldowns_suspend_but_connect_request_remains_negotiation() {
        let now = Instant::now();
        let mut g = PublishGovernor::default();
        for secs in [0, 2, 6] {
            g.observe("r", &reject("x"), now + Duration::from_secs(secs));
        }
        let t = now + Duration::from_secs(6);
        assert!(g.suspended(t));
        assert_eq!(g.relays["r"].rate, 0.5);
        add(&mut g, "low", "discovery", t);
        add(&mut g, "request", "request", t);
        assert_eq!(g.poll("r", t), Some(frame("request")));
        assert!(g.poll("r", t + Duration::from_secs(2)).is_none());
        assert_eq!(g.poll("r", t + QUIET_PERIOD), Some(frame("low")));
        assert!(!g.suspended(t + QUIET_PERIOD));
    }
    #[test]
    fn equal_priority_round_robins_destinations_and_due_retries() {
        let now = Instant::now();
        let mut g = PublishGovernor::default();
        for (id, target) in [
            ("a1", "a"),
            ("a2", "a"),
            ("a3", "a"),
            ("b1", "b"),
            ("c1", "c"),
        ] {
            g.register(id, frame(id), "offer", target, now);
            g.enqueue("r", frame(id), now);
        }
        assert_eq!(g.poll("r", now), Some(frame("a1")));
        g.observe("r", &reject("a1"), now);
        let t = now + Duration::from_secs(2);
        for id in ["b1", "c1", "a2"] {
            assert_eq!(g.poll("r", t), Some(frame(id)));
        }
        // Refill for this check: a new B target must be served before A's backlog.
        g.register("b2", frame("b2"), "offer", "b", t);
        g.enqueue("r", frame("b2"), t);
        assert_eq!(g.poll("r", t + Duration::from_secs(1)), Some(frame("b2")));
    }
    #[test]
    fn idle_writer_has_no_deadline_and_enqueued_or_rejected_work_has_one() {
        let now = Instant::now();
        let mut g = PublishGovernor::default();
        g.open("r", now);
        assert!(g.next_wakeup("r", now).is_none());
        add(&mut g, "a", "offer", now);
        assert_eq!(g.next_wakeup("r", now), Some(Duration::ZERO));
        g.poll("r", now);
        assert!(g.next_wakeup("r", now).is_none());
        g.observe("r", &reject("a"), now);
        assert_eq!(g.next_wakeup("r", now), Some(Duration::from_secs(1)));
    }
    #[test]
    fn retries_are_bounded_and_acceptance_on_another_relay_cancels() {
        let now = Instant::now();
        let mut g = PublishGovernor::default();
        add(&mut g, "a", "offer", now);
        assert!(g.poll("r", now).is_some());
        let mut t = now;
        for delay in NEGOTIATION_RETRY {
            g.observe("r", &reject("a"), t);
            assert!(g.poll("r", t + delay - Duration::from_millis(1)).is_none());
            t += delay;
            assert_eq!(g.poll("r", t), Some(frame("a")));
        }
        g.observe("r", &reject("a"), t);
        assert!(g.poll("r", t + Duration::from_secs(60)).is_none());
        add(&mut g, "b", "answer", t);
        g.poll("r", t + Duration::from_secs(60));
        g.observe("r", &reject("b"), t + Duration::from_secs(60));
        g.observe(
            "other",
            &RelayMessage::Ok {
                event_id: "b".into(),
                accepted: true,
                message: String::new(),
            },
            t,
        );
        assert!(g.poll("r", t + Duration::from_secs(70)).is_none());
    }
    #[test]
    fn stale_attempt_session_and_permanent_denial_never_retry() {
        let now = Instant::now();
        let mut g = PublishGovernor::default();
        add(&mut g, "a", "candidate", now);
        g.poll("r", now);
        g.observe("r", &reject("a"), now);
        g.end_attempt("a");
        assert!(g.poll("r", now + Duration::from_secs(2)).is_none());
        add(&mut g, "b", "answer", now);
        g.reset_session();
        assert!(g.poll("r", now + Duration::from_secs(10)).is_none());
        add(&mut g, "c", "offer", now + Duration::from_secs(10));
        g.poll("r", now + Duration::from_secs(10));
        for reason in [
            "blocked:",
            "restricted:",
            "invalid:",
            "pow:",
            "auth-required:",
        ] {
            g.observe(
                "r",
                &RelayMessage::Ok {
                    event_id: "c".into(),
                    accepted: false,
                    message: reason.into(),
                },
                now,
            );
        }
        assert!(g.poll("r", now + Duration::from_secs(20)).is_none());
    }
}
