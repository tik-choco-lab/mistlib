use std::collections::HashMap;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};
use web_time::{Duration, Instant};

/// Generic time-boxed cache: entries expire after `ttl` (checked via
/// [`TtlCache::sweep`]), and the total entry count is capped at
/// `max_entries` by evicting the oldest entry when the cap would
/// otherwise be exceeded (see [`TtlCache::evict_oldest_until_below_limit`]).
///
/// Holds the shared TTL + max-entries eviction logic used by
/// `overlay::router::dedupe::OverlaySeenCache` and
/// `signaling::nostr::dedupe::DedupeCache`, which differ only in key type
/// and in cache-specific policy layered on top (e.g. sentinel-value
/// handling, default capacity). Derefs to the underlying map so callers
/// can use standard `HashMap` methods (`contains_key`, `insert`, `len`, ...)
/// directly.
#[derive(Debug)]
pub(crate) struct TtlCache<K> {
    ttl: Duration,
    max_entries: usize,
    entries: HashMap<K, Instant>,
}

impl<K: Eq + Hash + Clone> TtlCache<K> {
    pub(crate) fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries,
            entries: HashMap::new(),
        }
    }

    /// Removes all entries whose age (relative to `now`) has reached `ttl`.
    pub(crate) fn sweep(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.entries
            .retain(|_, inserted_at| now.duration_since(*inserted_at) < ttl);
    }

    /// Repeatedly removes the oldest entry (by insertion/update time) until
    /// the cache is below `max_entries`.
    pub(crate) fn evict_oldest_until_below_limit(&mut self) {
        while self.entries.len() >= self.max_entries {
            let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, inserted_at)| **inserted_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.entries.remove(&oldest_key);
        }
    }
}

impl<K> Deref for TtlCache<K> {
    type Target = HashMap<K, Instant>;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl<K> DerefMut for TtlCache<K> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entries
    }
}
