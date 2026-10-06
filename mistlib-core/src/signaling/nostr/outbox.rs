//! Bounded discovery work shared by native and browser pacing workers.
use std::collections::VecDeque;
use web_time::{Duration, Instant};

const CAPACITY: usize = 32;
pub const DISCOVERY_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiscoveryPriority {
    Reply,
    Probe,
    Ordinary,
}

#[derive(Debug)]
pub struct DiscoveryRequest {
    pub pubkey: String,
    pub priority: DiscoveryPriority,
    delay: Duration,
    queued_at: Instant,
}

#[derive(Debug, Default)]
pub struct DiscoveryOutbox {
    queue: VecDeque<DiscoveryRequest>,
    last_send: Option<Instant>,
    pub worker_epoch: Option<u64>,
}

impl DiscoveryOutbox {
    /// Promote waiting work without duplicating an in-flight reservation.
    pub fn promote_reply(&mut self, pubkey: &str) -> bool {
        if let Some(item) = self.queue.iter_mut().find(|item| item.pubkey == pubkey) {
            item.priority = DiscoveryPriority::Reply;
            item.delay = Duration::ZERO;
            true
        } else {
            false
        }
    }

    /// Return the evicted reservation, including the new item if all work has
    /// higher priority. Prefer dropping the oldest ordinary response.
    pub fn push(
        &mut self,
        pubkey: String,
        priority: DiscoveryPriority,
        delay: Duration,
        now: Instant,
    ) -> Option<String> {
        let dropped = if self.queue.len() == CAPACITY {
            let lowest = self.queue.iter().map(|item| item.priority).max().unwrap();
            if priority > lowest {
                return Some(pubkey);
            }
            let index = self
                .queue
                .iter()
                .position(|item| item.priority == lowest)
                .unwrap();
            Some(self.queue.remove(index).unwrap().pubkey)
        } else {
            None
        };
        self.queue.push_back(DiscoveryRequest {
            pubkey,
            priority,
            delay,
            queued_at: now,
        });
        dropped
    }

    fn next_index(&self) -> Option<usize> {
        self.queue
            .iter()
            .enumerate()
            .min_by_key(|(_, item)| item.priority)
            .map(|(index, _)| index)
    }

    pub fn delay(&self, now: Instant) -> Option<Duration> {
        let item = &self.queue[self.next_index()?];
        let deadline = if item.priority == DiscoveryPriority::Ordinary {
            self.last_send.unwrap_or(item.queued_at) + item.delay.max(DISCOVERY_INTERVAL)
        } else {
            self.last_send.map_or(now, |last| last + DISCOVERY_INTERVAL)
        };
        Some(deadline.saturating_duration_since(now))
    }

    pub fn pop(&mut self, now: Instant) -> Option<DiscoveryRequest> {
        if !self.delay(now)?.is_zero() {
            return None;
        }
        let index = self.next_index()?;
        self.last_send = Some(now);
        self.queue.remove(index)
    }

    /// Anchor the next interval after the actual enqueue, including time spent
    /// waiting on publication ordering or a congested relay writer.
    pub fn sent(&mut self, now: Instant) {
        self.last_send = Some(now);
    }

    pub fn stop_probes(&mut self) -> Vec<String> {
        let dropped = self
            .queue
            .iter()
            .filter(|item| item.priority == DiscoveryPriority::Probe)
            .map(|item| item.pubkey.clone())
            .collect();
        self.queue
            .retain(|item| item.priority != DiscoveryPriority::Probe);
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_peer_reply_promotes_without_duplicate_or_eviction() {
        let now = Instant::now();
        let mut outbox = DiscoveryOutbox::default();
        for i in 0..CAPACITY {
            outbox.push(
                i.to_string(),
                DiscoveryPriority::Ordinary,
                Duration::from_millis(900),
                now,
            );
        }
        assert!(outbox.promote_reply("31"));
        assert_eq!(outbox.queue.len(), CAPACITY);
        assert_eq!(outbox.pop(now).unwrap().pubkey, "31");
        assert!(!outbox.promote_reply("31"));
        assert_eq!(outbox.queue.len(), CAPACITY - 1);
        assert_eq!(outbox.queue.front().unwrap().pubkey, "0");
        assert!(outbox.pop(now).is_none());
    }

    #[test]
    fn overflow_priority_and_pacing_share_one_limit() {
        let now = Instant::now();
        let mut outbox = DiscoveryOutbox::default();
        for i in 0..CAPACITY {
            assert!(outbox
                .push(
                    i.to_string(),
                    DiscoveryPriority::Ordinary,
                    Duration::from_millis(900),
                    now
                )
                .is_none());
        }
        assert_eq!(
            outbox.push(
                "reply".into(),
                DiscoveryPriority::Reply,
                Duration::ZERO,
                now
            ),
            Some("0".into())
        );
        assert_eq!(outbox.pop(now).unwrap().pubkey, "reply");
        assert!(outbox.pop(now).is_none());
        assert!(outbox.pop(now + Duration::from_millis(899)).is_none());
        assert_eq!(
            outbox.pop(now + Duration::from_millis(900)).unwrap().pubkey,
            "1"
        );
        outbox.push(
            "probe".into(),
            DiscoveryPriority::Probe,
            Duration::ZERO,
            now,
        );
        assert!(outbox.pop(now + Duration::from_millis(999)).is_none());
        assert_eq!(
            outbox
                .pop(now + Duration::from_millis(1000))
                .unwrap()
                .pubkey,
            "probe"
        );
        assert_eq!(outbox.queue.len(), CAPACITY - 2);
    }
}
