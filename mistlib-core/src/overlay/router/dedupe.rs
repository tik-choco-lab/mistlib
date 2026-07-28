use crate::types::NodeId;
use crate::util::TtlCache;
use web_time::{Duration, Instant};

pub const OVERLAY_SEEN_TTL: Duration = Duration::from_secs(30);
pub const OVERLAY_SEEN_MAX_ENTRIES: usize = 4096;

#[derive(Debug)]
pub struct OverlaySeenCache {
    seen: TtlCache<(NodeId, u64)>,
}

impl OverlaySeenCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            seen: TtlCache::new(ttl, max_entries),
        }
    }

    pub fn check_and_insert(&mut self, from: &NodeId, msg_id: u64) -> bool {
        if msg_id == 0 {
            return true;
        }

        let now = Instant::now();
        self.sweep(now);
        let key = (from.clone(), msg_id);
        if self.seen.contains_key(&key) {
            return false;
        }

        self.seen.evict_oldest_until_below_limit();
        self.seen.insert(key, now);
        true
    }

    #[cfg(test)]
    pub fn check_and_insert_at(&mut self, from: &NodeId, msg_id: u64, now: Instant) -> bool {
        if msg_id == 0 {
            return true;
        }

        self.sweep(now);
        let key = (from.clone(), msg_id);
        if self.seen.contains_key(&key) {
            return false;
        }

        self.seen.evict_oldest_until_below_limit();
        self.seen.insert(key, now);
        true
    }

    pub fn sweep(&mut self, now: Instant) {
        self.seen.sweep(now);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.seen.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> NodeId {
        NodeId(id.to_string())
    }

    #[test]
    fn check_and_insert_rejects_duplicate_within_ttl() {
        let mut cache = OverlaySeenCache::new(Duration::from_secs(30), 4096);
        let from = node("peer-a");

        assert!(cache.check_and_insert(&from, 42));
        assert!(!cache.check_and_insert(&from, 42));
        assert!(cache.check_and_insert(&from, 43));
    }

    #[test]
    fn zero_msg_id_is_never_recorded() {
        let mut cache = OverlaySeenCache::new(Duration::from_secs(30), 4096);
        let from = node("peer-a");

        assert!(cache.check_and_insert(&from, 0));
        assert!(cache.check_and_insert(&from, 0));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn ttl_expiry_allows_reprocessing() {
        let mut cache = OverlaySeenCache::new(Duration::from_secs(1), 4096);
        let from = node("peer-a");
        let now = Instant::now();

        assert!(cache.check_and_insert_at(&from, 42, now));
        assert!(!cache.check_and_insert_at(&from, 42, now + Duration::from_millis(500)));
        assert!(cache.check_and_insert_at(&from, 42, now + Duration::from_secs(2)));
    }

    #[test]
    fn max_entries_evicts_oldest() {
        let mut cache = OverlaySeenCache::new(Duration::from_secs(60), 2);
        let from = node("peer-a");
        let now = Instant::now();

        assert!(cache.check_and_insert_at(&from, 1, now));
        assert!(cache.check_and_insert_at(&from, 2, now + Duration::from_millis(1)));
        assert!(cache.check_and_insert_at(&from, 3, now + Duration::from_millis(2)));

        assert_eq!(cache.len(), 2);
        assert!(cache.check_and_insert_at(&from, 1, now + Duration::from_millis(3)));
        assert!(!cache.check_and_insert_at(&from, 3, now + Duration::from_millis(4)));
    }
}
