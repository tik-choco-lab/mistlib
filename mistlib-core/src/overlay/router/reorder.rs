use crate::signaling::MessageContent;
use crate::types::NodeId;
use std::collections::{BTreeMap, HashMap};
use web_time::{Duration, Instant};

/// Maximum messages buffered per source before the gap is force-flushed.
pub const REORDER_MAX_PER_SOURCE: usize = 256;
/// Maximum number of distinct sources tracked before the oldest is evicted.
pub const REORDER_MAX_SOURCES: usize = 1024;
/// How long a gap may persist before buffered successors are flushed in seq order.
///
/// Must comfortably outlive transport-level reconnection: mistlib-native's ICE
/// restart grace period (`DISCONNECTED_GRACE_MS`, mistlib-native/src/transports/webrtc.rs)
/// is 5s, so a transient network blip can delay in-flight messages by close to
/// that long without them being lost for good. 8s leaves headroom above that
/// 5s grace period so a reconnecting peer's delayed message has a chance to
/// fill the gap before it's force-flushed and the late arrival is dropped.
pub const REORDER_GAP_TIMEOUT: Duration = Duration::from_secs(8);

struct SourceState {
    next_expected: u64,
    buffered: BTreeMap<u64, MessageContent>,
    /// When the current gap first appeared; drives the time-based flush.
    gap_since: Option<Instant>,
    last_activity: Instant,
}

impl SourceState {
    fn new(now: Instant) -> Self {
        Self {
            // The sender assigns per-destination seq starting at 1, so a fresh
            // source is expected to begin there. If the sender is actually mid-stream
            // (e.g. this receiver restarted while the sender kept counting), the gap
            // never fills and the cap/timeout stall protection re-baselines it.
            next_expected: 1,
            buffered: BTreeMap::new(),
            gap_since: None,
            last_activity: now,
        }
    }

    /// Delivers all buffered messages in seq order and advances past them.
    ///
    /// Returns the skipped seq range as `Some((next_expected, first_buffered))`
    /// when there was a gap between what was expected and the earliest
    /// buffered message (i.e. the half-open range `next_expected..first_buffered`
    /// was never delivered), or `None` if the flush was contiguous. Callers
    /// use this purely for observability (logging); behavior is unchanged.
    fn flush(&mut self, out: &mut Vec<MessageContent>) -> Option<(u64, u64)> {
        let skipped = self.buffered.keys().next().and_then(|&first| {
            if first > self.next_expected {
                Some((self.next_expected, first))
            } else {
                None
            }
        });
        if let Some((&max_seq, _)) = self.buffered.iter().next_back() {
            for (_, content) in std::mem::take(&mut self.buffered) {
                out.push(content);
            }
            self.next_expected = max_seq + 1;
        }
        self.gap_since = None;
        skipped
    }
}

/// Per-source reorder buffer that restores end-to-end ordering of `ReliableOrdered`
/// unicast messages after overlay route switches (relay <-> direct).
///
/// The timeout flush is driven lazily from message arrival: mistlib-core has no
/// background timer and is shared with WASM, so ordering never depends on a wall
/// clock being polled elsewhere. Memory is bounded per source and per source count;
/// delivery is never blocked indefinitely.
pub struct ReorderBuffer {
    sources: HashMap<NodeId, SourceState>,
    max_per_source: usize,
    max_sources: usize,
    gap_timeout: Duration,
}

impl ReorderBuffer {
    pub fn new(max_per_source: usize, max_sources: usize, gap_timeout: Duration) -> Self {
        Self {
            sources: HashMap::new(),
            max_per_source,
            max_sources,
            gap_timeout,
        }
    }

    /// Accepts an inbound message and returns the messages now deliverable, in order.
    pub fn accept(
        &mut self,
        from: &NodeId,
        seq: u64,
        content: MessageContent,
    ) -> Vec<MessageContent> {
        self.accept_at(from, seq, content, Instant::now())
    }

    fn accept_at(
        &mut self,
        from: &NodeId,
        seq: u64,
        content: MessageContent,
        now: Instant,
    ) -> Vec<MessageContent> {
        // seq == 0 means "no sequencing": deliver immediately, never buffer.
        // If this source already has a pending gap (from earlier ReliableOrdered
        // traffic), still drive its stale-gap flush so an unreliable/broadcast
        // arrival doesn't leave old buffered messages stranded past the timeout.
        // A source seen only via seq==0 must not get a SourceState allocated for
        // it, so this looks up rather than inserts.
        if seq == 0 {
            if let Some(state) = self.sources.get_mut(from) {
                state.last_activity = now;
                let mut out = Vec::new();
                if let Some(since) = state.gap_since {
                    if now.duration_since(since) >= self.gap_timeout {
                        let buffered_count = state.buffered.len();
                        if let Some((skip_from, skip_to)) = state.flush(&mut out) {
                            tracing::warn!(
                                "[Reorder] gap timeout (unreliable/broadcast arrival): forced flush from {} skipped seq {}..={} ({} buffered)",
                                from,
                                skip_from,
                                skip_to - 1,
                                buffered_count
                            );
                        }
                    }
                }
                out.push(content);
                return out;
            }
            return vec![content];
        }

        self.evict_sources_if_needed(from, now);

        let max_per_source = self.max_per_source;
        let gap_timeout = self.gap_timeout;
        let state = self
            .sources
            .entry(from.clone())
            .or_insert_with(|| SourceState::new(now));
        state.last_activity = now;

        let mut out = Vec::new();

        // Lazily flush a stale gap before processing the new arrival.
        if let Some(since) = state.gap_since {
            if now.duration_since(since) >= gap_timeout {
                let buffered_count = state.buffered.len();
                if let Some((skip_from, skip_to)) = state.flush(&mut out) {
                    tracing::warn!(
                        "[Reorder] gap timeout: forced flush from {} skipped seq {}..={} ({} buffered)",
                        from,
                        skip_from,
                        skip_to - 1,
                        buffered_count
                    );
                }
            }
        }

        if seq == 1 && state.next_expected > 1 {
            // The sender restarted its per-destination counter (process restart or
            // sender-side counter eviction). Without this, every message from the
            // restarted stream would look "late" and be dropped forever. Flush any
            // leftovers from the old stream, deliver, and re-baseline. True
            // duplicates of seq 1 are caught earlier by the (from, msg_id) dedup.
            let buffered_count = state.buffered.len();
            if let Some((skip_from, skip_to)) = state.flush(&mut out) {
                tracing::warn!(
                    "[Reorder] sender counter restart: re-baselining {} to seq 1, discarding leftover buffered seq {}..={} from the old stream ({} buffered)",
                    from,
                    skip_from,
                    skip_to - 1,
                    buffered_count
                );
            }
            out.push(content);
            state.next_expected = 2;
            return out;
        }

        if seq < state.next_expected {
            // Duplicate or late (dedup normally catches this); drop.
            tracing::warn!(
                "[Reorder] dropping late/duplicate message from {}: seq={} already past next_expected={}",
                from,
                seq,
                state.next_expected
            );
            return out;
        }

        if seq == state.next_expected {
            out.push(content);
            state.next_expected += 1;
            while let Some(next) = state.buffered.remove(&state.next_expected) {
                out.push(next);
                state.next_expected += 1;
            }
            state.gap_since = if state.buffered.is_empty() {
                None
            } else {
                Some(now)
            };
        } else {
            // seq > next_expected: hold until the gap fills.
            state.buffered.insert(seq, content);
            if state.gap_since.is_none() {
                state.gap_since = Some(now);
            }
            let buffered_count = state.buffered.len();
            if buffered_count > max_per_source {
                if let Some((skip_from, skip_to)) = state.flush(&mut out) {
                    tracing::warn!(
                        "[Reorder] buffer cap exceeded ({} > {}): forced flush from {} skipped seq {}..={}",
                        buffered_count,
                        max_per_source,
                        from,
                        skip_from,
                        skip_to - 1
                    );
                }
            }
        }

        out
    }

    /// Flushes every source whose gap has been open for at least the gap
    /// timeout, without waiting for new traffic from that source to trigger
    /// the lazy flush in `accept`/`accept_at`. A gapped source that goes idle
    /// (no more messages, not even `seq == 0` control traffic) would
    /// otherwise hold its buffered successors forever; this is the
    /// time-only escape hatch, meant to be polled periodically (e.g. from an
    /// engine's background tick) rather than driven by arrivals.
    ///
    /// Returns `(source, messages)` pairs, messages in seq order, for every
    /// source flushed. Sources with no pending gap, or a gap that hasn't yet
    /// reached the timeout, are left untouched.
    pub fn flush_expired(&mut self, now: Instant) -> Vec<(NodeId, Vec<MessageContent>)> {
        let gap_timeout = self.gap_timeout;
        let mut flushed = Vec::new();
        for (id, state) in self.sources.iter_mut() {
            let Some(since) = state.gap_since else {
                continue;
            };
            if now.duration_since(since) < gap_timeout {
                continue;
            }
            let buffered_count = state.buffered.len();
            let mut out = Vec::new();
            if let Some((skip_from, skip_to)) = state.flush(&mut out) {
                tracing::warn!(
                    "[Reorder] gap timeout (idle flush): forced flush from {} skipped seq {}..={} ({} buffered)",
                    id,
                    skip_from,
                    skip_to - 1,
                    buffered_count
                );
            }
            if !out.is_empty() {
                flushed.push((id.clone(), out));
            }
        }
        flushed
    }

    fn evict_sources_if_needed(&mut self, incoming: &NodeId, _now: Instant) {
        if self.sources.len() < self.max_sources || self.sources.contains_key(incoming) {
            return;
        }
        if let Some(oldest) = self
            .sources
            .iter()
            .min_by_key(|(_, s)| s.last_activity)
            .map(|(id, _)| id.clone())
        {
            self.sources.remove(&oldest);
        }
    }
}

impl Default for ReorderBuffer {
    fn default() -> Self {
        Self::new(
            REORDER_MAX_PER_SOURCE,
            REORDER_MAX_SOURCES,
            REORDER_GAP_TIMEOUT,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn node(id: &str) -> NodeId {
        NodeId(id.to_string())
    }

    fn raw(tag: &str) -> MessageContent {
        MessageContent::Raw(Bytes::copy_from_slice(tag.as_bytes()))
    }

    /// Gap timeout used by tests that need a concrete, short threshold to
    /// straddle with simulated `now` offsets (e.g. 500ms early / 2s late).
    /// Deliberately independent of `REORDER_GAP_TIMEOUT` (the production
    /// default, 8s) so these tests don't have to track that value.
    const TEST_GAP_TIMEOUT: Duration = Duration::from_secs(1);

    fn tags(contents: &[MessageContent]) -> Vec<String> {
        contents
            .iter()
            .map(|c| match c {
                MessageContent::Raw(b) => String::from_utf8_lossy(b).to_string(),
                _ => "<non-raw>".to_string(),
            })
            .collect()
    }

    #[test]
    fn out_of_order_arrival_is_reordered() {
        let mut buf = ReorderBuffer::default();
        let src = node("peer-a");
        let now = Instant::now();

        // First seen seq is 1 -> baseline. Arrive 2, 1, 3.
        let d2 = buf.accept_at(&src, 2, raw("m2"), now);
        assert!(d2.is_empty(), "2 buffered while waiting for 1");

        let d1 = buf.accept_at(&src, 1, raw("m1"), now);
        assert_eq!(tags(&d1), ["m1", "m2"], "1 unblocks buffered 2");

        let d3 = buf.accept_at(&src, 3, raw("m3"), now);
        assert_eq!(tags(&d3), ["m3"]);
    }

    #[test]
    fn zero_seq_bypasses_buffer() {
        let mut buf = ReorderBuffer::default();
        let src = node("peer-a");
        let now = Instant::now();

        // Establish a gap with seq-ordered traffic.
        assert!(buf.accept_at(&src, 5, raw("m5"), now).is_empty());
        // A seq==0 message must deliver immediately despite the pending gap.
        let d = buf.accept_at(&src, 0, raw("ctrl"), now);
        assert_eq!(tags(&d), ["ctrl"]);
    }

    #[test]
    fn zero_seq_flushes_stale_gap_for_known_source() {
        let mut buf = ReorderBuffer::new(
            REORDER_MAX_PER_SOURCE,
            REORDER_MAX_SOURCES,
            TEST_GAP_TIMEOUT,
        );
        let src = node("peer-a");
        let now = Instant::now();

        // 1 delivers, then 3 arrives with 2 missing: a gap that never fills.
        assert_eq!(tags(&buf.accept_at(&src, 1, raw("m1"), now)), ["m1"]);
        assert!(buf.accept_at(&src, 3, raw("m3"), now).is_empty());

        // A seq==0 arrival after the stale threshold must flush the buffered
        // gap (in seq order) before delivering its own content.
        let d = buf.accept_at(&src, 0, raw("ctrl"), now + Duration::from_secs(2));
        assert_eq!(tags(&d), ["m3", "ctrl"]);
    }

    #[test]
    fn zero_seq_from_unknown_source_does_not_allocate_state() {
        let mut buf = ReorderBuffer::default();
        let src = node("peer-a");
        let now = Instant::now();

        let d = buf.accept_at(&src, 0, raw("ctrl"), now);
        assert_eq!(tags(&d), ["ctrl"]);
        assert!(
            !buf.sources.contains_key(&src),
            "a seq==0-only source must not get a SourceState entry"
        );
    }

    #[test]
    fn zero_seq_within_stale_window_does_not_flush_early() {
        let mut buf = ReorderBuffer::new(
            REORDER_MAX_PER_SOURCE,
            REORDER_MAX_SOURCES,
            TEST_GAP_TIMEOUT,
        );
        let src = node("peer-a");
        let now = Instant::now();

        assert_eq!(tags(&buf.accept_at(&src, 1, raw("m1"), now)), ["m1"]);
        assert!(buf.accept_at(&src, 3, raw("m3"), now).is_empty());

        // Still within the gap timeout: seq==0 delivers its own content only,
        // leaving the buffered gap (m3) untouched.
        let d = buf.accept_at(&src, 0, raw("ctrl"), now + Duration::from_millis(500));
        assert_eq!(tags(&d), ["ctrl"]);

        // The gap is still pending and flushes normally once the real timeout
        // arrives via subsequent seq-ordered traffic.
        let flushed = buf.accept_at(&src, 4, raw("m4"), now + Duration::from_secs(2));
        assert_eq!(tags(&flushed), ["m3", "m4"]);
    }

    #[test]
    fn duplicate_seq_is_dropped() {
        let mut buf = ReorderBuffer::default();
        let src = node("peer-a");
        let now = Instant::now();

        assert_eq!(tags(&buf.accept_at(&src, 1, raw("m1"), now)), ["m1"]);
        assert_eq!(tags(&buf.accept_at(&src, 2, raw("m2"), now)), ["m2"]);
        assert_eq!(tags(&buf.accept_at(&src, 3, raw("m3"), now)), ["m3"]);
        // Re-delivery of an already-delivered seq is dropped. seq 1 is excluded:
        // it signals a sender counter restart; true duplicates of seq 1 are
        // caught upstream by the (from, msg_id) dedup cache.
        assert!(buf.accept_at(&src, 2, raw("m2-dup"), now).is_empty());
        assert!(buf.accept_at(&src, 3, raw("m3-dup"), now).is_empty());
    }

    #[test]
    fn gap_flushes_after_timeout() {
        let mut buf = ReorderBuffer::new(
            REORDER_MAX_PER_SOURCE,
            REORDER_MAX_SOURCES,
            TEST_GAP_TIMEOUT,
        );
        let src = node("peer-a");
        let now = Instant::now();

        // 1 delivers, then 3 arrives with 2 missing (the gap that never fills).
        assert_eq!(tags(&buf.accept_at(&src, 1, raw("m1"), now)), ["m1"]);
        assert!(buf.accept_at(&src, 3, raw("m3"), now).is_empty());

        // Before the timeout, 3 stays buffered.
        let early = buf.accept_at(&src, 5, raw("m5"), now + Duration::from_millis(500));
        assert!(early.is_empty());

        // After the timeout, the next arrival flushes the buffered successors in seq
        // order (3, 5) and re-baselines; the contiguous trigger (6) then delivers too.
        let flushed = buf.accept_at(&src, 6, raw("m6"), now + Duration::from_secs(2));
        assert_eq!(tags(&flushed), ["m3", "m5", "m6"]);
    }

    #[test]
    fn buffer_cap_is_enforced() {
        let mut buf = ReorderBuffer::new(4, REORDER_MAX_SOURCES, REORDER_GAP_TIMEOUT);
        let src = node("peer-a");
        let now = Instant::now();

        // Baseline 1 delivered; then fill the gap past the cap without seq 2.
        assert_eq!(tags(&buf.accept_at(&src, 1, raw("m1"), now)), ["m1"]);
        for s in 3..=6u64 {
            assert!(buf
                .accept_at(&src, s, raw(&format!("m{s}")), now)
                .is_empty());
        }
        // The 5th buffered entry exceeds cap=4 and forces a flush.
        let flushed = buf.accept_at(&src, 7, raw("m7"), now);
        assert_eq!(tags(&flushed), ["m3", "m4", "m5", "m6", "m7"]);
    }

    #[test]
    fn ordering_is_independent_per_source() {
        let mut buf = ReorderBuffer::default();
        let a = node("peer-a");
        let b = node("peer-b");
        let now = Instant::now();

        // A: out of order, B: in order. They must not interfere.
        assert!(buf.accept_at(&a, 2, raw("a2"), now).is_empty());
        assert_eq!(tags(&buf.accept_at(&b, 1, raw("b1"), now)), ["b1"]);
        assert_eq!(tags(&buf.accept_at(&a, 1, raw("a1"), now)), ["a1", "a2"]);
        assert_eq!(tags(&buf.accept_at(&b, 2, raw("b2"), now)), ["b2"]);
    }

    #[test]
    fn sender_counter_restart_rebaselines() {
        let mut buf = ReorderBuffer::default();
        let src = node("peer-a");
        let now = Instant::now();

        // Old stream progressed well past 1.
        assert_eq!(tags(&buf.accept_at(&src, 1, raw("m1"), now)), ["m1"]);
        assert_eq!(tags(&buf.accept_at(&src, 2, raw("m2"), now)), ["m2"]);
        // Leftover gap from the old stream.
        assert!(buf.accept_at(&src, 4, raw("m4"), now).is_empty());

        // Sender restarts at seq 1: old leftovers flush, new stream re-baselines.
        let d = buf.accept_at(&src, 1, raw("n1"), now);
        assert_eq!(tags(&d), ["m4", "n1"]);
        assert_eq!(tags(&buf.accept_at(&src, 2, raw("n2"), now)), ["n2"]);
    }

    #[test]
    fn flush_expired_delivers_buffered_tail_after_timeout_with_no_new_traffic() {
        let mut buf = ReorderBuffer::new(
            REORDER_MAX_PER_SOURCE,
            REORDER_MAX_SOURCES,
            TEST_GAP_TIMEOUT,
        );
        let src = node("peer-a");
        let now = Instant::now();

        // 1 delivers, then 3 arrives with 2 missing: a gap that never fills
        // because the source goes silent (no further traffic at all, not
        // even seq == 0 control messages).
        assert_eq!(tags(&buf.accept_at(&src, 1, raw("m1"), now)), ["m1"]);
        assert!(buf.accept_at(&src, 3, raw("m3"), now).is_empty());

        let flushed = buf.flush_expired(now + Duration::from_secs(2));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].0, src);
        assert_eq!(tags(&flushed[0].1), ["m3"]);

        // The gap is cleared: a fresh in-order arrival delivers immediately
        // rather than being held for a stale gap that no longer exists.
        assert_eq!(tags(&buf.accept_at(&src, 4, raw("m4"), now)), ["m4"]);
    }

    #[test]
    fn flush_expired_delivers_nothing_before_timeout() {
        let mut buf = ReorderBuffer::new(
            REORDER_MAX_PER_SOURCE,
            REORDER_MAX_SOURCES,
            TEST_GAP_TIMEOUT,
        );
        let src = node("peer-a");
        let now = Instant::now();

        assert_eq!(tags(&buf.accept_at(&src, 1, raw("m1"), now)), ["m1"]);
        assert!(buf.accept_at(&src, 3, raw("m3"), now).is_empty());

        // Still within the gap timeout: nothing is flushed yet.
        let flushed = buf.flush_expired(now + Duration::from_millis(500));
        assert!(flushed.is_empty());

        // The buffered message is still there once the real timeout arrives.
        let flushed = buf.flush_expired(now + Duration::from_secs(2));
        assert_eq!(tags(&flushed[0].1), ["m3"]);
    }

    #[test]
    fn flush_expired_ignores_sources_with_no_pending_gap() {
        let mut buf = ReorderBuffer::default();
        let src = node("peer-a");
        let now = Instant::now();

        // In-order traffic only: no gap ever opens.
        assert_eq!(tags(&buf.accept_at(&src, 1, raw("m1"), now)), ["m1"]);
        assert_eq!(tags(&buf.accept_at(&src, 2, raw("m2"), now)), ["m2"]);

        assert!(buf.flush_expired(now + Duration::from_secs(2)).is_empty());
    }

    #[test]
    fn source_eviction_bounds_memory() {
        let mut buf = ReorderBuffer::new(REORDER_MAX_PER_SOURCE, 2, REORDER_GAP_TIMEOUT);
        let now = Instant::now();

        buf.accept_at(&node("a"), 5, raw("a"), now);
        buf.accept_at(&node("b"), 5, raw("b"), now + Duration::from_millis(1));
        // Third distinct source evicts the oldest (a).
        buf.accept_at(&node("c"), 5, raw("c"), now + Duration::from_millis(2));
        assert_eq!(buf.sources.len(), 2);
        assert!(!buf.sources.contains_key(&node("a")));
        assert!(buf.sources.contains_key(&node("c")));
    }
}
