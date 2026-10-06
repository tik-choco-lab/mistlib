//! Delivery budget for identifying Requests, independent of discovery bindings.
use std::collections::BTreeMap;
use web_time::{Duration, Instant};

const MAX_RETRIES: u32 = 3;
const MAX_SESSION_RETRIES: usize = 12;
// Retain completed entries so repeated Requests cannot restart their budget.

#[derive(Debug, Default)]
pub struct DiscoveryExchanges {
    peers: BTreeMap<String, Exchange>,
    retries: usize,
    sent_retries: usize,
}

#[derive(Debug)]
struct Exchange {
    next: Instant,
    retries: u32,
    progressed: bool,
    queued: bool,
    evidence: (bool, u64),
    first_advertisement: Option<u64>,
    cap_logged: bool,
}

impl DiscoveryExchanges {
    /// Reserve an initial publish or a due retry before enqueueing/scheduling.
    /// Failed local enqueues still consume the budget, just like lost events.
    pub fn request(&mut self, pubkey: &str, now: Instant) -> bool {
        if let Some(peer) = self.peers.get_mut(pubkey) {
            if peer.queued || peer.progressed || now < peer.next {
                return false;
            }
            if peer.retries >= MAX_RETRIES || self.retries >= MAX_SESSION_RETRIES {
                if !peer.cap_logged {
                    tracing::info!(
                        "Nostr discovery retries stopped peer={} reason=cap",
                        pubkey.chars().take(8).collect::<String>()
                    );
                    peer.cap_logged = true;
                }
                return false;
            }
            peer.retries += 1;
            self.retries += 1;
            peer.next = now + retry_delay(peer.retries);
            return true;
        }
        self.peers.insert(
            pubkey.to_owned(),
            Exchange {
                next: now + retry_delay(0),
                retries: 0,
                progressed: false,
                queued: false,
                evidence: (false, 0),
                first_advertisement: None,
                cap_logged: false,
            },
        );
        true
    }

    /// The caller publishes these reservations without reserving them again.
    pub fn poll(&mut self, now: Instant) -> Vec<String> {
        self.poll_with_evidence(now, |_| (false, 0))
    }

    pub fn poll_with_evidence(
        &mut self,
        now: Instant,
        evidence: impl Fn(&str) -> (bool, u64),
    ) -> Vec<String> {
        let mut keys: Vec<_> = self.peers.keys().cloned().collect();
        keys.sort_by_key(|key| {
            let peer = &self.peers[key];
            let hint = evidence(key);
            // All verified sources share the signed event clock. Refresh
            // history only breaks ties; old evidence cannot stay on top.
            let latest = peer.evidence.1.max(hint.1);
            (
                std::cmp::Reverse((latest, peer.evidence.0 || hint.0)),
                peer.retries,
            )
        });
        let mut protected = 0;
        let mut due = Vec::new();
        for key in keys {
            let peer = &self.peers[&key];
            if peer.progressed || peer.retries >= MAX_RETRIES {
                continue;
            }
            // Leave one future reservation for each higher-ranked pending
            // peer, including an initial Request still waiting in the outbox.
            if MAX_SESSION_RETRIES - self.retries <= protected {
                if self.retries == MAX_SESSION_RETRIES {
                    self.request(&key, now);
                }
                break;
            }
            if self.request(&key, now) {
                due.push(key);
            } else {
                protected += 1;
            }
        }
        due
    }

    /// Copy bootstrap evidence while its bounded selection window is open.
    /// Store hints only for existing exchanges, without another identity map.
    pub fn observe_evidence(&mut self, pubkey: &str, evidence: (bool, u64)) {
        if let Some(peer) = self.peers.get_mut(pubkey) {
            peer.evidence.0 |= evidence.0;
            peer.evidence.1 = peer.evidence.1.max(evidence.1);
            if evidence.1 > 0 {
                peer.first_advertisement.get_or_insert(evidence.1);
            }
        }
    }

    pub fn observe_advertisement(&mut self, pubkey: &str, created_at: u64, joined_at: Option<u64>) {
        if let Some(peer) = self.peers.get_mut(pubkey) {
            let first = peer.first_advertisement.get_or_insert(created_at);
            *first = (*first).min(created_at);
            peer.evidence.1 = peer.evidence.1.max(created_at);
            // Same refresh-span rule as bootstrap, including legacy peers.
            peer.evidence.0 |= peer.evidence.1.saturating_sub(*first) >= 30
                || joined_at.is_some_and(|joined| {
                    joined > 0 && created_at.saturating_sub(joined / 1000) >= 30
                });
        }
    }

    pub fn queued(&mut self, pubkey: &str) {
        if let Some(peer) = self.peers.get_mut(pubkey) {
            peer.queued = true;
        }
    }

    pub fn is_queued(&self, pubkey: &str) -> bool {
        self.peers
            .get(pubkey)
            .is_some_and(|peer| peer.queued && !peer.progressed)
    }

    /// Log successful relay enqueues only; local failures still spend retries.
    pub fn log_sent(&mut self, pubkey: &str) {
        if let Some(peer) = self.peers.get(pubkey) {
            let prefix = pubkey.chars().take(8).collect::<String>();
            tracing::debug!("Nostr paced discovery sent peer={prefix}");
            if peer.retries > 0 {
                self.sent_retries += 1;
                tracing::info!(
                    "Nostr discovery retry sent peer={prefix} attempt={}/3 session={}/12",
                    peer.retries,
                    self.sent_retries
                );
            }
        }
    }

    pub fn observe_activity(&mut self, pubkey: &str, created_at: u64) {
        if let Some(peer) = self.peers.get_mut(pubkey) {
            peer.evidence.1 = peer.evidence.1.max(created_at);
            if let Some(first) = peer.first_advertisement {
                peer.evidence.0 |= peer.evidence.1.saturating_sub(first) >= 30;
            }
        }
    }

    pub fn sent(&mut self, pubkey: &str, now: Instant) {
        if let Some(peer) = self.peers.get_mut(pubkey) {
            peer.queued = false;
            peer.next = now + retry_delay(peer.retries);
        }
    }

    /// Unsent work must not charge a retry or suppress a repeated Request.
    pub fn release(&mut self, pubkey: &str) {
        if let Some(peer) = self.peers.get_mut(pubkey) {
            if peer.retries == 0 && !peer.progressed {
                self.peers.remove(pubkey);
            } else {
                peer.queued = false;
                if peer.retries > 0 {
                    peer.retries -= 1;
                    self.retries -= 1;
                }
                peer.next = Instant::now();
            }
        }
    }

    pub fn progress(&mut self, pubkey: &str, now: Instant) {
        // SDP may precede discovery. Remember it to suppress a later reply.
        if !self.peers.contains_key(pubkey) {
            self.peers.insert(
                pubkey.to_owned(),
                Exchange {
                    next: now,
                    retries: 0,
                    progressed: true,
                    queued: false,
                    evidence: (false, 0),
                    first_advertisement: None,
                    cap_logged: false,
                },
            );
        }
        if let Some(peer) = self.peers.get_mut(pubkey) {
            if !peer.progressed {
                tracing::info!(
                    "Nostr discovery retries stopped peer={} reason=progressed",
                    pubkey.chars().take(8).collect::<String>()
                );
            }
            peer.progressed = true;
        }
    }

    pub fn pending(&self, pubkey: &str) -> bool {
        self.peers.get(pubkey).is_some_and(|peer| !peer.progressed)
    }
}

fn retry_delay(retries: u32) -> Duration {
    Duration::from_millis(1_500 << retries.min(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_cap_and_progress_are_independent_of_binding() {
        let now = Instant::now();
        let mut state = DiscoveryExchanges::default();
        assert!(state.request("peer", now));
        for ms in [1499, 4499, 10499] {
            assert!(state.poll(now + Duration::from_millis(ms)).is_empty());
            assert_eq!(
                state.poll(now + Duration::from_millis(ms + 1)),
                vec!["peer"]
            );
        }
        assert!(state.poll(now + Duration::from_secs(100)).is_empty());
        assert!(state.request("connected", now));
        state.progress("connected", now);
        assert!(!state.request("connected", now + Duration::from_secs(100)));
        state.progress("sdp-first", now);
        assert!(!state.request("sdp-first", now));
    }

    #[test]
    fn total_budget_is_bounded_for_entire_session() {
        let now = Instant::now();
        let mut state = DiscoveryExchanges::default();
        for i in 0..100 {
            assert!(state.request(&i.to_string(), now));
        }
        assert_eq!(
            state.poll(now + Duration::from_secs(2)).len(),
            MAX_SESSION_RETRIES
        );
        assert!(state.poll(now + Duration::from_secs(100)).is_empty());
        assert!(!state.request("0", now + Duration::from_secs(100)));
    }

    #[test]
    fn evicted_work_releases_reservations_and_backoff_starts_on_send() {
        let now = Instant::now();
        let mut state = DiscoveryExchanges::default();
        assert!(state.request("peer", now));
        state.queued("peer");
        assert!(state.poll(now + Duration::from_secs(100)).is_empty());
        state.release("peer");
        assert!(state.request("peer", now));
        state.queued("peer");
        state.sent("peer", now + Duration::from_secs(20));
        assert!(state.poll(now + Duration::from_millis(21499)).is_empty());
        assert_eq!(state.poll(now + Duration::from_millis(21500)), vec!["peer"]);
        state.queued("peer");
        state.release("peer");
        assert_eq!(state.retries, 0);
    }

    #[test]
    fn recent_inbound_and_fewest_retries_win_due_budget() {
        let now = Instant::now();
        let mut state = DiscoveryExchanges::default();
        for i in 0..20 {
            state.request(&i.to_string(), now);
        }
        state.observe_activity("19", 1);
        assert_eq!(state.poll(now + Duration::from_secs(2))[0], "19");
        let mut state = DiscoveryExchanges::default();
        state.request("a", now);
        state.request("a", now + Duration::from_secs(2));
        state.request("z", now);
        assert_eq!(state.poll(now + Duration::from_secs(10)), vec!["z", "a"]);
        for activity in [false, true] {
            let mut state = DiscoveryExchanges::default();
            for i in 0..20 {
                state.request(&i.to_string(), now);
            }
            state.observe_advertisement("19", 400, Some(if activity { 400_000 } else { 100_000 }));
            if activity {
                state.observe_activity("19", 900);
            }
            assert_eq!(
                state.poll(now + Duration::from_secs(2))[0],
                "19",
                "refresh and mailbox evidence survives bootstrap completion"
            );
        }
    }
}

#[cfg(test)]
mod retry_ranking {
    use super::*;

    #[test]
    fn stale_inbound_does_not_starve_recent_active_provider() {
        let now = Instant::now();
        let mut state = DiscoveryExchanges::default();
        for i in 0..12 {
            let dead = format!("dead-{i}");
            state.request(&dead, now);
            state.observe_activity(&dead, 1000);
        }
        state.request("live", now);
        state.observe_advertisement("live", 1000, Some(900_000));
        state.observe_activity("live", 1089);
        let due = state.poll(now + Duration::from_secs(90));
        assert!(
            due.contains(&"live".to_owned()),
            "ninety-second-old inbound exhausted the budget ahead of current signed activity"
        );
    }

    #[test]
    fn dead_peers_do_not_starve_live_provider_retry() {
        let now = Instant::now();
        let mut state = DiscoveryExchanges::default();
        for i in 0..12 {
            assert!(state.request(&format!("{i:064x}"), now));
        }
        let live = "f".repeat(64);
        assert!(state.request(&live, now));
        let first = state.poll_with_evidence(now + Duration::from_secs(2), |key| (key == live, 0));
        assert_eq!(first.len(), 12);
        assert!(first.contains(&live));
        // All first replies were lost; a live provider must retain a repair chance.
        assert!(
            !state.request(&live, now + Duration::from_secs(100)),
            "session cap remains twelve"
        );
    }
}
