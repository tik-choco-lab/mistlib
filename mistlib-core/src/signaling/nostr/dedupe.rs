use crate::util::TtlCache;
use web_time::{Duration, Instant};

const DEFAULT_MAX_ENTRIES: usize = 4096;

#[derive(Debug)]
pub struct DedupeCache {
    seen: TtlCache<String>,
}

impl DedupeCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            seen: TtlCache::new(ttl, DEFAULT_MAX_ENTRIES),
        }
    }

    #[cfg(test)]
    fn with_max_entries(ttl: Duration, max_entries: usize) -> Self {
        Self {
            seen: TtlCache::new(ttl, max_entries),
        }
    }

    pub fn check_and_insert(&mut self, event_id: &str) -> bool {
        let now = Instant::now();
        self.sweep(now);
        if self.seen.contains_key(event_id) {
            return false;
        }
        self.seen.evict_oldest_until_below_limit();
        self.seen.insert(event_id.to_string(), now);
        true
    }

    pub fn sweep(&mut self, now: Instant) {
        self.seen.sweep(now);
    }

    pub fn clear(&mut self) {
        self.seen.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_and_insert_rejects_duplicate_within_ttl() {
        let mut cache = DedupeCache::new(Duration::from_secs(60));

        assert!(cache.check_and_insert("event-1"));
        assert!(!cache.check_and_insert("event-1"));
    }

    #[test]
    fn sweep_removes_entries_older_than_ttl() {
        let mut cache = DedupeCache::new(Duration::from_secs(1));
        let now = Instant::now();

        cache
            .seen
            .insert("expired".to_string(), now - Duration::from_secs(2));
        cache
            .seen
            .insert("fresh".to_string(), now - Duration::from_millis(500));

        cache.sweep(now);

        assert!(!cache.seen.contains_key("expired"));
        assert!(cache.seen.contains_key("fresh"));
    }

    #[test]
    fn check_and_insert_caps_entries_by_evicting_oldest() {
        let mut cache = DedupeCache::with_max_entries(Duration::from_secs(60), 2);
        let now = Instant::now();

        cache
            .seen
            .insert("oldest".to_string(), now - Duration::from_secs(2));
        cache
            .seen
            .insert("newer".to_string(), now - Duration::from_secs(1));

        assert!(cache.check_and_insert("newest"));

        assert_eq!(cache.seen.len(), 2);
        assert!(!cache.seen.contains_key("oldest"));
        assert!(cache.seen.contains_key("newer"));
        assert!(cache.seen.contains_key("newest"));
    }
}
