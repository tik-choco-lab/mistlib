//! Bounded escape from a discovery ring containing unexpired, dead identities.
use super::DiscoveryTable;
use std::collections::{HashMap, HashSet};
use web_time::{Duration, Instant};

const INITIAL_DELAY: Duration = Duration::from_millis(500);
const MAX_ROUNDS: u32 = 2;
const MAX_PROBES: usize = 6;
pub const TARGET_PEERS: usize = 2;
// Same-second duplicates and brief join-time bursts are not refresh evidence.
const MIN_REFRESH_SPAN: u64 = 30;

/// Per-signaling-session bootstrap budget. Advertisements cannot restart it.
/// Time is supplied by the caller so deadline tests need no sleeping.
#[derive(Debug, Default)]
pub struct DiscoveryBootstrap {
    next_probe: Option<Instant>,
    rounds: u32,
    finished: bool,
    advertisements: HashMap<String, Advertisement>,
    probed: HashSet<String>,
}

#[derive(Debug)]
struct Advertisement {
    first: u64,
    latest: u64,
    refreshed: bool,
}

impl DiscoveryBootstrap {
    pub fn stop(&mut self) {
        self.finished = true;
        self.advertisements.clear();
    }

    pub fn observe(&mut self, pubkey: &str, created_at: u64, joined_at: Option<u64>, now: Instant) {
        if self.finished || self.rounds >= MAX_ROUNDS {
            return;
        }
        self.next_probe.get_or_insert(now + INITIAL_DELAY);
        let ad = self
            .advertisements
            .entry(pubkey.to_owned())
            .or_insert(Advertisement {
                first: created_at,
                latest: created_at,
                refreshed: false,
            });
        ad.first = ad.first.min(created_at);
        ad.latest = ad.latest.max(created_at);
        // joined_at is Unix milliseconds; Nostr created_at is Unix seconds.
        // A signed refresh carries session age even when the relay returns only
        // one event. Legacy peers can establish the same hint through distinct
        // timestamps. This is a ranking hint, not proof of current liveness.
        ad.refreshed |= joined_at.is_some_and(|joined| {
            joined > 0 && created_at.saturating_sub(joined / 1000) >= MIN_REFRESH_SPAN
        }) || ad.latest - ad.first >= MIN_REFRESH_SPAN;
    }

    /// A verified room-mailbox event may be encrypted for another peer. Its
    /// signed timestamp still provides activity evidence for an already admitted
    /// discovery candidate. It cannot add a candidate or restart the budget.
    pub fn observe_activity(&mut self, pubkey: &str, created_at: u64) {
        if self.finished || self.rounds >= MAX_ROUNDS {
            return;
        }
        if let Some(ad) = self.advertisements.get_mut(pubkey) {
            ad.latest = ad.latest.max(created_at);
            ad.refreshed |= ad.latest.saturating_sub(ad.first) >= MIN_REFRESH_SPAN;
        }
    }

    /// Reuse signed refresh/activity evidence for delivery retry ranking.
    pub fn evidence(&self, pubkey: &str) -> (bool, u64) {
        self.advertisements
            .get(pubkey)
            .map(|ad| (ad.refreshed, ad.latest))
            .unwrap_or_default()
    }

    /// Front-load four alternate probes, then at most two more. Stop permanently
    /// once the normal two-peer target is healthy. A signaling reply alone is
    /// not a usable connection: stalled ICE must not disable this escape hatch.
    pub fn poll(
        &mut self,
        now: Instant,
        table: &mut DiscoveryTable,
        local_pubkey: &str,
        local_rank: &str,
        requested: &HashSet<String>,
        connected: usize,
    ) -> Vec<String> {
        if connected >= TARGET_PEERS {
            self.finished = true;
            self.advertisements.clear();
        }
        if self.finished || self.rounds >= MAX_ROUNDS {
            return Vec::new();
        }
        let Some(deadline) = self.next_probe else {
            return Vec::new();
        };
        if now < deadline {
            return Vec::new();
        }
        // Preserve the forward ring as the tie-breaker; only this bounded
        // fallback uses refresh evidence. Normal topology ranks never change.
        let count = table.active_pubkeys().len();
        let mut candidates =
            table.responder_pubkeys_for(local_pubkey, local_rank, local_pubkey, local_rank, count);
        candidates.reverse();
        candidates.retain(|key| !requested.contains(key) && !self.probed.contains(key));
        self.advertisements
            .retain(|key, _| table.expires_at_for_pubkey(key).is_some());
        candidates.sort_by_key(|key| {
            std::cmp::Reverse(
                self.advertisements
                    .get(key)
                    .map(|ad| (ad.refreshed, ad.latest))
                    .unwrap_or_default(),
            )
        });
        let batch = if connected > 0 {
            1
        } else if self.rounds == 0 {
            4
        } else {
            2
        };
        candidates.truncate(batch.min(MAX_PROBES - self.probed.len()));
        // Remember attempts even if publishing fails or a rejoin clears the
        // caller's requested set. Do not reject late answers: simply never
        // spend another fallback slot on that identity during this session.
        self.probed.extend(candidates.iter().cloned());
        // Charge even empty rounds so advertisements cannot restart the budget.
        self.rounds += 1;
        if self.rounds == MAX_ROUNDS {
            // Keep at most six selected hints until delivery reservations copy them.
            self.advertisements
                .retain(|key, _| self.probed.contains(key));
        }
        self.next_probe = Some(now + INITIAL_DELAY);
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> (Instant, DiscoveryTable, DiscoveryBootstrap) {
        let now = Instant::now();
        let mut table = DiscoveryTable::default();
        let mut retry = DiscoveryBootstrap::default();
        for i in 1..=10 {
            let key = format!("{i:02}");
            table.insert_pubkey_with_rank(key.clone(), u64::MAX, key.clone());
            retry.observe(&key, 1000 + i, Some((1000 + i) * 1000 + 123), now);
        }
        (now, table, retry)
    }

    #[test]
    fn dead_fresh_candidates_do_not_hide_older_refreshing_provider() {
        for joined_at in [Some(100_000), None] {
            let (now, mut table, mut retry) = snapshot();
            table.insert_pubkey_with_rank("provider".into(), u64::MAX, "11".into());
            retry.observe("provider", 400, joined_at, now);
            if joined_at.is_none() {
                retry.observe("provider", 450, None, now);
            }
            assert_eq!(
                table.responder_pubkeys_for("provider", "11", "local", "00", 2),
                vec!["10", "09"]
            );
            let requested = HashSet::from(["10".into(), "09".into()]);
            assert_eq!(
                retry.poll(
                    now + INITIAL_DELAY,
                    &mut table,
                    "local",
                    "00",
                    &requested,
                    0
                ),
                vec!["provider", "08", "07", "06"]
            );
        }
    }

    #[test]
    fn duplicate_and_brief_advertisements_do_not_gain_refresh_priority() {
        let (now, mut table, mut retry) = snapshot();
        for _ in 0..10 {
            retry.observe("01", 1001, Some(1_001_123), now);
        }
        retry.observe("02", 1003, Some(1_002_123), now);
        retry.observe("03", 1003, Some(0), now);
        retry.observe("04", 1004, Some(2_000_000), now);
        assert_eq!(
            retry.poll(
                now + INITIAL_DELAY,
                &mut table,
                "00",
                "00",
                &HashSet::new(),
                0
            ),
            vec!["10", "09", "08", "07"]
        );
    }

    #[test]
    fn front_loaded_deadlines_cap_and_failed_probe_exclusion() {
        let (now, mut table, mut retry) = snapshot();
        let requested = HashSet::new(); // Failed publishes / cleared requests.
        for (ms, expected) in [
            (499, vec![]),
            (500, vec!["10", "09", "08", "07"]),
            (999, vec![]),
            (1000, vec!["06", "05"]),
            (10000, vec![]),
        ] {
            assert_eq!(
                retry.poll(
                    now + Duration::from_millis(ms),
                    &mut table,
                    "00",
                    "00",
                    &requested,
                    0
                ),
                expected
            );
        }
        assert_eq!(retry.probed.len(), MAX_PROBES);
        assert_eq!(retry.advertisements.len(), MAX_PROBES);
        assert_eq!(retry.evidence("06"), (false, 1006));
        retry.observe("01", 2000, Some(1), now + Duration::from_secs(100));
        assert!(retry
            .poll(
                now + Duration::from_secs(100),
                &mut table,
                "00",
                "00",
                &requested,
                0
            )
            .is_empty());
    }

    #[test]
    fn health_stops_probes_permanently_even_before_first_deadline() {
        for healthy_at in [0, 750] {
            let (now, mut table, mut retry) = snapshot();
            let requested = HashSet::new();
            if healthy_at > 0 {
                assert_eq!(
                    retry
                        .poll(now + INITIAL_DELAY, &mut table, "00", "00", &requested, 1)
                        .len(),
                    1
                );
            }
            assert!(retry
                .poll(
                    now + Duration::from_millis(healthy_at),
                    &mut table,
                    "00",
                    "00",
                    &requested,
                    2
                )
                .is_empty());
            retry.observe("01", 2000, Some(1), now + Duration::from_secs(2));
            assert!(retry
                .poll(
                    now + Duration::from_secs(10),
                    &mut table,
                    "00",
                    "00",
                    &requested,
                    0
                )
                .is_empty());
        }
    }

    #[test]
    fn empty_and_expired_snapshots_are_bounded() {
        let now = Instant::now();
        let mut retry = DiscoveryBootstrap::default();
        let mut table = DiscoveryTable::default();
        assert!(retry
            .poll(now, &mut table, "local", "00", &HashSet::new(), 0)
            .is_empty());
        table.insert_pubkey("expired".into(), 1);
        retry.observe("expired", 0, None, now);
        for at in [1, 2, 100] {
            assert!(retry
                .poll(
                    now + Duration::from_secs(at),
                    &mut table,
                    "local",
                    "00",
                    &HashSet::new(),
                    0
                )
                .is_empty());
        }
        assert_eq!(retry.rounds, MAX_ROUNDS);
    }
    #[test]
    fn sustained_signed_mailbox_activity_prioritizes_provider_before_readvertisement() {
        let (now, mut table, mut retry) = snapshot();
        table.insert_pubkey_with_rank("provider".into(), u64::MAX, "11".into());
        retry.observe("provider", 400, Some(400_123), now);
        retry.observe_activity("provider", 900);
        retry.observe_activity("01", 1002);
        retry.observe_activity("unknown", 2000);
        assert!(!retry.advertisements.contains_key("unknown"));
        assert_eq!(
            retry.poll(
                now + INITIAL_DELAY,
                &mut table,
                "00",
                "00",
                &HashSet::new(),
                0
            ),
            vec!["provider", "10", "09", "08"]
        );
    }
}
