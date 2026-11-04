//! SPEC-23 recovery policy. Clocks and jitter samples belong to the adapter.
use crate::types::NodeId;
use std::collections::HashMap;
use web_time::{Duration, Instant};

/// Phase A U3 found an unbounded nine-second retry loop. Start gently for
/// transient faults, then cap at one minute to meet the S5 recovery target.
pub const RECOVERY_INITIAL: Duration = Duration::from_secs(3);
/// Preserve the existing finite CONNECT_REQUEST budget; intervals now share B5.
pub const CONNECT_REQUEST_SEND_LIMIT: u32 = 10;
pub const RECOVERY_MAX: Duration = Duration::from_secs(60);
/// B6: one failed-pair relay initiation per node, with no accumulated burst.
pub const FAILED_PAIR_INTERVAL: Duration = Duration::from_secs(60);
/// Phase A's discovery budget must survive identity rotation. A ten-minute
/// node window matches the default discovery TTL without reviving every reset.
pub const DISCOVERY_BUDGET_WINDOW: Duration = Duration::from_secs(600);

#[derive(Debug, Default)]
pub struct RecoveryBackoff {
    failures: u32,
}
impl RecoveryBackoff {
    /// `jitter` is a caller-supplied uniform sample in [0, 1].
    pub fn next_delay(&mut self, jitter: f64) -> Duration {
        let delay = self.delay(jitter);
        self.fired();
        delay
    }
    pub fn delay(&self, jitter: f64) -> Duration {
        let seconds = (RECOVERY_INITIAL
            .as_secs()
            .saturating_mul(1 << self.failures.min(5)))
        .min(RECOVERY_MAX.as_secs());
        Duration::from_secs_f64(seconds as f64 * (0.8 + 0.4 * jitter.clamp(0.0, 1.0)))
    }
    pub fn fired(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }
    pub fn connected(&mut self) {
        self.failures = 0;
    }
}

/// One entry per node, shared by active offer and CONNECT_REQUEST initiation.
/// Received offers bypass this gate. Entries survive transport cleanup/rebind.
#[derive(Debug, Default)]
pub struct PairBackoff {
    peers: HashMap<NodeId, PairRetry>,
    relay_retry_at: Option<Instant>,
}
#[derive(Debug, Default)]
struct PairRetry {
    backoff: RecoveryBackoff,
    deadline: Option<Instant>,
    unanswered: u32,
    token: Option<String>,
    local_ready: bool,
}
impl PairBackoff {
    pub fn failed(&mut self, node: &NodeId, now: Instant, jitter: f64) {
        let entry = self.peers.entry(node.clone()).or_default();
        let delay = entry.backoff.next_delay(jitter);
        entry.deadline = Some(now + delay);
        entry.unanswered = 0;
        entry.local_ready = false;
        tracing::info!(
            "[PairBackoff] action=extend node={} delay_ms={}",
            node,
            delay.as_millis()
        );
    }
    /// A real ICE attempt consumes the pending nudge, preserving failures.
    pub fn started(&mut self, node: &NodeId) {
        if let Some(entry) = self.peers.get_mut(node) {
            entry.deadline = None;
            entry.unanswered = 0;
            entry.local_ready = false;
        }
    }

    /// Keep stable identities recoverable even after discovery exchanges have
    /// progressed. Higher IDs nudge, lower IDs initiate locally. Missing
    /// offers back off with a finite budget; active attempts are skipped.
    pub fn poll_nudges(
        &mut self,
        local: &NodeId,
        now: Instant,
        jitter: f64,
        busy: &std::collections::HashSet<NodeId>,
        overlay: &std::collections::HashSet<NodeId>,
    ) -> Vec<NodeId> {
        let mut due = Vec::new();
        // Keep original deadlines for denied peers. Earliest expiry wins;
        // node ID breaks ties independently of HashMap iteration order.
        let mut waiting: Vec<_> = self
            .peers
            .iter()
            .filter_map(|(node, entry)| entry.deadline.map(|at| (at, node.clone())))
            .collect();
        waiting.sort_by(|(a_at, a_node), (b_at, b_node)| {
            a_at.cmp(b_at).then_with(|| a_node.0.cmp(&b_node.0))
        });
        for (_, node) in waiting {
            let entry = self.peers.get_mut(&node).expect("queued pair exists");
            if &node == local || busy.contains(&node) || entry.deadline.is_none_or(|at| at > now) {
                continue;
            }
            if local.0 > node.0 && entry.unanswered >= CONNECT_REQUEST_SEND_LIMIT {
                tracing::info!("[PairBackoff] action=skip node={node} reason=nudge_limit");
                entry.deadline = None;
                continue;
            }
            if (entry.backoff.failures > 0 || entry.unanswered > 0) && !overlay.contains(&node) {
                if self.relay_retry_at.is_some_and(|at| now < at) {
                    continue;
                }
                self.relay_retry_at = Some(now + FAILED_PAIR_INTERVAL);
                tracing::info!("[PairBackoff] action=admit node={node} interval_ms=60000");
            }
            if entry.unanswered > 0 {
                entry.backoff.fired();
            }
            let delay = entry.backoff.delay(jitter);
            // A due local connect may pass admission while the next autonomous
            // retry is still bounded if capacity/dispatcher declines the attempt.
            entry.local_ready = local.0 < node.0;
            entry.deadline = Some(now + delay);
            entry.unanswered += 1;
            tracing::info!(
                "[PairBackoff] action=retry node={} next_delay_ms={}",
                node,
                delay.as_millis()
            );
            due.push(node.clone());
        }
        due
    }

    pub fn nodes(&self) -> Vec<NodeId> {
        self.peers.keys().cloned().collect()
    }
    /// Autonomous connect calls cannot bypass the queue after a failure.
    /// A low-ID queue selection reserves `local_ready` until PC creation.
    /// Inbound offers never call this; inbound Requests use `response=true`.
    pub fn can_initiate(&self, node: &NodeId, now: Instant, overlay: bool, response: bool) -> bool {
        if response {
            return true;
        }
        let Some(entry) = self.peers.get(node) else {
            return true;
        };
        if entry.local_ready {
            return true;
        }
        entry.deadline.is_some_and(|at| at <= now) && (entry.backoff.failures == 0 || overlay)
    }

    /// B7: adapters inspect the state map under its write lock before adding
    /// Connected. Only a zero-to-one transition resets the other failed pairs.
    /// Jitter is supplied by the adapter; ID ordering spreads a batch over
    /// 100..1000ms without reading a clock or random source here.
    pub fn connection_established(
        &mut self,
        node: &NodeId,
        isolated_before: bool,
        now: Instant,
        jitter: f64,
    ) {
        self.connected(node);
        if !isolated_before {
            return;
        }
        let mut nodes: Vec<_> = self
            .peers
            .iter()
            .filter(|(_, entry)| entry.backoff.failures > 0)
            .map(|(node, _)| node.clone())
            .collect();
        nodes.sort_by(|a, b| a.0.cmp(&b.0));
        let count = nodes.len().max(1) as f64;
        for (index, node) in nodes.into_iter().enumerate() {
            let entry = self.peers.get_mut(&node).expect("failed pair exists");
            entry.backoff.connected();
            entry.unanswered = 0;
            entry.local_ready = false;
            let phase = (jitter.clamp(0.0, 1.0) + index as f64 / count).fract();
            entry.deadline = Some(now + Duration::from_secs_f64(0.1 + 0.9 * phase));
            tracing::info!("[PairBackoff] action=route_recovered node={node}");
        }
    }
    /// Expired bindings remove intent; new authenticated evidence re-arms an
    /// exhausted nudge budget without resetting the pair's failure backoff.
    pub fn refresh_binding(&mut self, node: &NodeId, token: Option<String>, now: Instant) {
        let Some(token) = token else {
            if self.peers.remove(node).is_some() {
                tracing::info!("[PairBackoff] action=skip node={node} reason=binding_expired");
            }
            return;
        };
        if let Some(entry) = self.peers.get_mut(node) {
            if entry.token.as_ref() != Some(&token) {
                if entry.token.is_some() && entry.deadline.is_none() {
                    entry.deadline = Some(now + entry.backoff.delay(0.5));
                }
                entry.token = Some(token);
                entry.unanswered = 0;
            }
        }
    }

    pub fn remaining(&self, node: &NodeId, now: Instant) -> Duration {
        self.peers
            .get(node)
            .filter(|e| !e.local_ready)
            .and_then(|e| e.deadline)
            .map_or(Duration::ZERO, |at| at.saturating_duration_since(now))
    }
    pub fn connected(&mut self, node: &NodeId) {
        if self.peers.remove(node).is_some() {
            tracing::debug!("[PairBackoff] action=reset node={node}");
        }
    }
}

/// Rotation is armed by actual connectivity, consumed only by a real reset.
#[derive(Debug, Default)]
pub struct IdentityRecovery {
    connected_since_rotation: bool,
}
impl IdentityRecovery {
    pub fn connected(&mut self) {
        self.connected_since_rotation = true;
    }
    pub fn take_rotation(&mut self) -> bool {
        std::mem::take(&mut self.connected_since_rotation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deterministic_backoff_and_reset() {
        let mut b = RecoveryBackoff::default();
        for secs in [3, 6, 12, 24, 48, 60, 60] {
            assert_eq!(b.next_delay(0.5), Duration::from_secs(secs));
        }
        b.connected();
        assert_eq!(b.next_delay(0.0), Duration::from_millis(2400));
        b.connected();
        assert_eq!(b.next_delay(1.0), Duration::from_millis(3600));
    }
    #[test]
    fn pairs_are_independent_and_offers_need_no_admission() {
        let now = Instant::now();
        let a = NodeId("a".into());
        let b = NodeId("b".into());
        let mut p = PairBackoff::default();
        p.failed(&a, now, 0.5);
        assert_eq!(
            p.remaining(&a, now + Duration::from_secs(2)),
            Duration::from_secs(1)
        );
        assert!(p.remaining(&b, now).is_zero());
        p.failed(&a, now + Duration::from_secs(3), 0.5);
        assert_eq!(
            p.remaining(&a, now + Duration::from_secs(4)),
            Duration::from_secs(5)
        );
        p.connected(&a);
        assert!(p.remaining(&a, now).is_zero());
    }
    #[test]
    fn failed_handshakes_never_arm_rotation() {
        let mut p = IdentityRecovery::default();
        assert!(!p.take_rotation());
        p.connected();
        assert!(p.take_rotation());
        assert!(!p.take_rotation());
    }
}

#[cfg(test)]
mod nudge_tests {
    use super::*;
    use std::collections::HashSet;
    #[test]
    fn stable_identity_keeps_retrying_after_discovery_has_progressed() {
        let now = Instant::now();
        let low = NodeId("a".into());
        let high = NodeId("z".into());
        let mut pair = PairBackoff::default();
        pair.failed(&low, now, 0.5);
        let busy = HashSet::new();
        assert!(pair
            .poll_nudges(
                &high,
                now + Duration::from_secs(2),
                0.5,
                &busy,
                &pair.nodes().into_iter().collect()
            )
            .is_empty());
        for seconds in [3, 9, 21, 45, 93, 153] {
            assert_eq!(
                pair.poll_nudges(
                    &high,
                    now + Duration::from_secs(seconds),
                    0.5,
                    &busy,
                    &pair.nodes().into_iter().collect()
                ),
                vec![low.clone()]
            );
        }
        pair.started(&low);
        assert!(pair
            .poll_nudges(
                &high,
                now + Duration::from_secs(1000),
                0.5,
                &busy,
                &pair.nodes().into_iter().collect()
            )
            .is_empty());
        pair.failed(&low, now + Duration::from_secs(1000), 0.5);
        pair.connected(&low);
        assert!(pair
            .poll_nudges(
                &high,
                now + Duration::from_secs(2000),
                0.5,
                &busy,
                &pair.nodes().into_iter().collect()
            )
            .is_empty());
    }
    #[test]
    fn nudges_respect_busy_and_direction_and_preserve_actual_failure_count() {
        let now = Instant::now();
        let low = NodeId("a".into());
        let high = NodeId("z".into());
        let mut pair = PairBackoff::default();
        pair.failed(&low, now, 0.5);
        assert!(pair
            .poll_nudges(
                &high,
                now + Duration::from_secs(3),
                0.5,
                &HashSet::from([low.clone()]),
                &pair.nodes().into_iter().collect(),
            )
            .is_empty());
        assert_eq!(
            pair.poll_nudges(
                &high,
                now + Duration::from_secs(3),
                0.5,
                &HashSet::new(),
                &pair.nodes().into_iter().collect()
            ),
            vec![low.clone()]
        );
        pair.started(&low);
        pair.failed(&low, now + Duration::from_secs(4), 0.5);
        assert_eq!(
            pair.remaining(&low, now + Duration::from_secs(4)),
            Duration::from_secs(6)
        );
        pair.failed(&high, now, 0.5);
        assert_eq!(
            pair.poll_nudges(
                &low,
                now + Duration::from_secs(100),
                0.5,
                &HashSet::new(),
                &pair.nodes().into_iter().collect()
            ),
            vec![high.clone()]
        );
        assert!(pair
            .remaining(&high, now + Duration::from_secs(100))
            .is_zero());
    }
    #[test]
    fn unanswered_nudges_stop_and_resume_only_with_new_binding_evidence() {
        let now = Instant::now();
        let node = NodeId("a".into());
        let local = NodeId("z".into());
        let mut p = PairBackoff::default();
        p.failed(&node, now, 0.5);
        p.refresh_binding(&node, Some("first".into()), now);
        for n in 1..=CONNECT_REQUEST_SEND_LIMIT {
            assert_eq!(
                p.poll_nudges(
                    &local,
                    now + Duration::from_secs(u64::from(n) * 100),
                    0.5,
                    &Default::default(),
                    &p.nodes().into_iter().collect(),
                ),
                vec![node.clone()]
            );
        }
        let later = now + Duration::from_secs(2000);
        assert!(p
            .poll_nudges(
                &local,
                later,
                0.5,
                &Default::default(),
                &p.nodes().into_iter().collect()
            )
            .is_empty());
        p.refresh_binding(&node, Some("first".into()), later);
        assert!(p
            .poll_nudges(
                &local,
                later,
                0.5,
                &Default::default(),
                &p.nodes().into_iter().collect()
            )
            .is_empty());
        p.refresh_binding(&node, Some("fresh-request".into()), later);
        assert_eq!(
            p.poll_nudges(
                &local,
                later + Duration::from_secs(60),
                0.5,
                &Default::default(),
                &p.nodes().into_iter().collect(),
            ),
            vec![node.clone()]
        );
        p.refresh_binding(&node, None, later);
        assert!(p.nodes().is_empty());
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn one_or_thirty_failed_pairs_share_one_slot_and_all_get_a_turn() {
        for count in [1, 30] {
            for local in [NodeId("!".into()), NodeId("z".into())] {
                let now = Instant::now();
                let nodes: Vec<_> = (0..count).map(|n| NodeId(format!("peer-{n:02}"))).collect();
                let mut pairs = PairBackoff::default();
                for node in nodes.iter().rev() {
                    pairs.failed(node, now, 0.5);
                }
                let mut admitted = Vec::new();
                let mut last = None;
                // Synthetic clock: two complete rounds, checking every second.
                for second in 0..(count * 120) {
                    let at = now + Duration::from_secs(second);
                    let due = pairs.poll_nudges(&local, at, 0.5, &HashSet::new(), &HashSet::new());
                    assert!(due.len() <= 1);
                    for node in due {
                        if let Some(previous) = last {
                            assert!(second - previous >= 60);
                        }
                        last = Some(second);
                        assert!(pairs.can_initiate(&node, at, false, local.0 > node.0));
                        pairs.started(&node);
                        pairs.failed(&node, at, 0.5);
                        admitted.push(node);
                    }
                }
                assert_eq!(
                    admitted,
                    nodes
                        .iter()
                        .cycle()
                        .take(count as usize * 2)
                        .cloned()
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn oldest_expiry_wins_and_denial_does_not_move_deadlines() {
        let now = Instant::now();
        let local = NodeId("z".into());
        let a = NodeId("a".into());
        let b = NodeId("b".into());
        let mut pairs = PairBackoff::default();
        pairs.failed(&a, now + Duration::from_secs(1), 0.5);
        pairs.failed(&b, now, 0.5);
        let at = now + Duration::from_secs(10);
        let original = pairs.peers[&a].deadline;
        assert_eq!(
            pairs.poll_nudges(&local, at, 0.5, &HashSet::new(), &HashSet::new()),
            vec![b]
        );
        assert!(pairs
            .poll_nudges(
                &local,
                at + Duration::from_secs(59),
                0.5,
                &HashSet::new(),
                &HashSet::new()
            )
            .is_empty());
        assert_eq!(pairs.peers[&a].deadline, original);
        assert_eq!(
            pairs.poll_nudges(
                &local,
                at + Duration::from_secs(60),
                0.5,
                &HashSet::new(),
                &HashSet::new()
            ),
            vec![a]
        );
    }

    #[test]
    fn first_attempt_overlay_and_passive_responses_do_not_spend_relay_slots() {
        let now = Instant::now();
        let local = NodeId("!".into());
        let a = NodeId("a".into());
        let b = NodeId("b".into());
        let fresh = NodeId("new".into());
        let mut pairs = PairBackoff::default();
        pairs.failed(&a, now, 0.5);
        pairs.failed(&b, now, 0.5);
        assert!(pairs.can_initiate(&fresh, now, false, false));
        assert!(pairs.can_initiate(&a, now, false, true));
        assert!(
            !pairs.can_initiate(&a, now, true, false),
            "overlay still obeys pair delay"
        );
        let at = now + Duration::from_secs(3);
        assert!(
            !pairs.can_initiate(&a, at, false, false),
            "autonomous calls must use queue"
        );
        assert!(pairs.can_initiate(&a, at, true, false));
        assert_eq!(
            pairs.poll_nudges(
                &local,
                at,
                0.5,
                &HashSet::new(),
                &HashSet::from([a.clone(), b.clone()])
            ),
            vec![a.clone(), b.clone()]
        );
        assert!(pairs.relay_retry_at.is_none());
        pairs.started(&a);
        pairs.failed(&a, at, 0.5);
        assert_eq!(
            pairs
                .poll_nudges(
                    &local,
                    at + Duration::from_secs(6),
                    0.5,
                    &HashSet::new(),
                    &HashSet::new()
                )
                .len(),
            1
        );
        // A used relay slot does not block a new peer or passive response.
        assert!(pairs.can_initiate(&fresh, at, false, false));
        assert!(pairs.can_initiate(&a, now, false, true));
    }

    #[test]
    fn only_first_connected_after_isolation_resets_failures_with_small_jitter() {
        let now = Instant::now();
        let first = NodeId("first".into());
        let a = NodeId("a".into());
        let b = NodeId("b".into());
        let mut pairs = PairBackoff::default();
        for node in [&first, &a, &b] {
            for _ in 0..8 {
                pairs.failed(node, now, 0.5);
            }
        }
        let at = now + Duration::from_secs(1);
        pairs.connection_established(&first, true, at, 0.5);
        assert!(!pairs.peers.contains_key(&first));
        for node in [&a, &b] {
            assert_eq!(pairs.peers[node].backoff.failures, 0);
            let delay = pairs.remaining(node, at);
            assert!(delay >= Duration::from_millis(100) && delay <= Duration::from_secs(1));
        }
        assert_ne!(pairs.peers[&a].deadline, pairs.peers[&b].deadline);
        assert_eq!(
            pairs.poll_nudges(
                &NodeId("!".into()),
                at + Duration::from_secs(1),
                0.5,
                &HashSet::new(),
                &HashSet::from([a.clone(), b.clone()])
            ),
            vec![b.clone(), a.clone()]
        );
        assert!(pairs.relay_retry_at.is_none());
        pairs.failed(&b, at, 0.5);
        let deadline = pairs.peers[&b].deadline;
        pairs.connection_established(&a, false, at, 0.5);
        pairs.connection_established(&a, false, at, 0.5); // duplicate Connected
        assert_eq!(pairs.peers[&b].backoff.failures, 1);
        assert_eq!(pairs.peers[&b].deadline, deadline);
        pairs.connection_established(&a, true, at, 0.5); // a later isolation can recover again
        assert_eq!(pairs.peers[&b].backoff.failures, 0);
    }
}
