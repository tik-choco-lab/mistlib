//! Delay-targeted bulk admission. Time and buffer observations are supplied by
//! adapters: browser release is local drain; native release includes ACKs.
use std::collections::{HashMap, VecDeque};

pub const MAX_BULK_BUDGET: u32 = 1024 * 1024;
pub const SAMPLE_INTERVAL_MS: u64 = 50;
/// Storage chunks plus envelope headroom; grow this floor for larger frames.
pub const INITIAL_MAX_FRAME: u32 = 17 * 1024;
/// Allow ACK-clocked flight without imposing seconds of queueing on slow links.
/// Faster links grow from delivery observations and near-full probes.
const NATIVE_MIN_BUDGET: u32 = 64 * 1024;
const NATIVE_RATE_WINDOW_MS: u64 = 2000;
const NATIVE_FALLBACK_RTT_MS: f64 = 100.0;

#[derive(Clone, Copy)]
struct Sample {
    generation: u64,
    at_ms: u64,
    submitted: u64,
    buffered: u64,
    backlogged: bool,
}

pub struct BulkPacer {
    target_ms: u32,
    native: bool,
    max_frame: u32,
    previous: Option<Sample>,
    rate: Option<f64>,
    budget: u32,
    rtt_min_ms: Option<f64>,
    delivery_rates: VecDeque<(u64, f64)>,
}
impl BulkPacer {
    pub fn new(target_ms: u32, native: bool, max_frame: u32) -> Self {
        let mut pacer = Self {
            target_ms,
            native,
            max_frame,
            previous: None,
            rate: None,
            budget: 64 * 1024,
            rtt_min_ms: None,
            delivery_rates: VecDeque::new(),
        };
        pacer.budget = pacer.clamp(pacer.budget);
        pacer
    }
    fn clamp(&self, bytes: u32) -> u32 {
        bytes.clamp(
            self.max_frame
                .saturating_mul(2)
                .max(if self.native { NATIVE_MIN_BUDGET } else { 0 })
                .min(MAX_BULK_BUDGET),
            MAX_BULK_BUDGET,
        )
    }
    pub fn budget(&self) -> u32 {
        self.budget
    }
    pub fn rate_bytes_per_second(&self) -> Option<f64> {
        self.rate
    }
    pub fn note_frame(&mut self, bytes: usize) {
        self.max_frame = self.max_frame.max(bytes.min(u32::MAX as usize) as u32);
        self.budget = self.clamp(self.budget);
    }
    pub fn note_rtt(&mut self, rtt_ms: f64) {
        if rtt_ms.is_finite() && rtt_ms > 0.0 {
            self.rtt_min_ms = Some(self.rtt_min_ms.map_or(rtt_ms, |old| old.min(rtt_ms)));
        }
    }
    /// Cumulative submissions must include ALL writes on the observed channel,
    /// including control on a reliable fallback. Idle intervals only rebaseline.
    pub fn observe(
        &mut self,
        generation: u64,
        at_ms: u64,
        submitted: u64,
        buffered: u64,
        backlogged: bool,
    ) {
        let sample = Sample {
            generation,
            at_ms,
            submitted,
            buffered,
            backlogged,
        };
        let Some(prev) = self.previous else {
            self.previous = Some(sample);
            return;
        };
        if generation != prev.generation || submitted < prev.submitted || at_ms < prev.at_ms {
            *self = Self::new(self.target_ms, self.native, self.max_frame);
            self.previous = Some(sample);
            return;
        }
        if !backlogged || !prev.backlogged {
            self.previous = Some(sample);
            return;
        }
        let elapsed = at_ms - prev.at_ms;
        // ACK releases are bursty. Span at least one RTT before estimating
        // native delivery; the observer can still refresh admission every tick.
        let rtt_ms = self.rtt_min_ms.unwrap_or(NATIVE_FALLBACK_RTT_MS);
        let sample_ms = if self.native {
            SAMPLE_INTERVAL_MS.max(rtt_ms.ceil() as u64)
        } else {
            SAMPLE_INTERVAL_MS
        };
        if elapsed < sample_ms {
            return;
        }
        let accepted = submitted - prev.submitted;
        let released = prev
            .buffered
            .saturating_add(accepted)
            .saturating_sub(buffered);
        let instant = released as f64 * 1000.0 / elapsed as f64;
        let rate = if self.native {
            // A budget-limited sample is only a lower bound on capacity. Keep
            // recent delivery peaks instead of feeding that limit into an EWMA.
            self.delivery_rates
                .retain(|(at, _)| at_ms - at < NATIVE_RATE_WINDOW_MS);
            self.delivery_rates.push_back((at_ms, instant));
            self.delivery_rates
                .iter()
                .map(|(_, rate)| *rate)
                .fold(0.0, f64::max)
        } else {
            let alpha = 1.0 - (-(elapsed as f64) / 1000.0).exp();
            self.rate
                .map_or(instant, |old| old + alpha * (instant - old))
        };
        let allowance_ms = f64::from(self.target_ms) + if self.native { rtt_ms } else { 0.0 };
        let estimated = self.clamp((rate * allowance_ms / 1000.0) as u32);
        // Native may never empty because its buffer includes flight. Probe
        // when the allowance is full and current delivery can sustain it;
        // retained peaks alone must not cause growth during a stall.
        let native_limited = self.native
            && buffered as f64 >= f64::from(self.budget) * 0.9
            && instant * allowance_ms / 1000.0 >= f64::from(self.budget) * 0.9;
        self.budget = if released > 0 && (buffered == 0 || native_limited) {
            self.clamp(estimated.max(self.budget.saturating_add(self.budget / 2)))
        } else {
            estimated
        };
        self.rate = Some(rate);
        self.previous = Some(sample);
    }
}

/// Adapter-neutral per-link accounting. A channel switch resets the estimator;
/// counter maps avoid mistaking fallback/control writes for drain.
pub struct BulkLink {
    pub policy: BulkPacer,
    submitted: HashMap<u64, u64>,
    channel: Option<u64>,
    buffered: u64,
}
impl BulkLink {
    pub fn new(target_ms: u32, native: bool) -> Self {
        Self {
            policy: BulkPacer::new(target_ms, native, INITIAL_MAX_FRAME),
            submitted: HashMap::new(),
            channel: None,
            buffered: 0,
        }
    }
    pub fn submitted(&mut self, channel: u64, bytes: usize, bulk: bool) {
        let counter = self.submitted.entry(channel).or_default();
        *counter = counter.saturating_add(bytes as u64);
        if self.channel == Some(channel) {
            self.buffered = self.buffered.saturating_add(bytes as u64);
        }
        if bulk {
            self.policy.note_frame(bytes);
        }
    }
    pub fn observe(&mut self, channel: u64, now_ms: u64, buffered: u64, backlogged: bool) {
        self.channel = Some(channel);
        self.buffered = buffered;
        self.policy.observe(
            channel,
            now_ms,
            *self.submitted.get(&channel).unwrap_or(&0),
            buffered,
            backlogged,
        );
    }
    pub fn can_submit(&self, channel: u64, bytes: usize) -> bool {
        self.channel == Some(channel)
            && (self.buffered.saturating_add(bytes as u64) <= u64::from(self.policy.budget())
                || self.buffered == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sample_interval_and_negative_release_are_conservative() {
        let mut p = BulkPacer::new(100, false, 1000);
        p.observe(1, 0, 0, 0, true);
        p.observe(1, 20, 5000, 5000, true);
        assert_eq!(p.rate, None);
        p.observe(1, 50, 5000, 10000, true);
        assert_eq!(p.rate, Some(0.0));
        assert_eq!(p.budget(), 2000);
    }
    #[test]
    fn config_defaults_and_camel_case_enable_pipeline() {
        // CP50 is the default since 2026-10-25; an explicit 0 opts out.
        let default: crate::config::StorageTransferConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(default.bulk_target_delay_ms, 50);
        let disabled: crate::config::StorageTransferConfig = serde_json::from_str(
            r#"{"unsequencedBulk":false,"controlPriority":false,"bulkChannel":false,"bulkTargetDelayMs":0}"#,
        )
        .unwrap();
        assert!(!crate::transport::send::pipeline_enabled(&disabled));
        let enabled: crate::config::StorageTransferConfig =
            serde_json::from_str(r#"{"bulkTargetDelayMs":100}"#).unwrap();
        assert!(crate::transport::send::pipeline_enabled(&enabled));
        assert_eq!(
            serde_json::to_value(enabled).unwrap()["bulkTargetDelayMs"],
            100
        );
    }
    #[test]
    fn rate_converges_and_stall_reduces_budget() {
        let mut p = BulkPacer::new(100, false, 1000);
        for i in 0..100 {
            p.observe(1, i * 50, i * 5000, 10000, true);
        }
        assert!((p.rate_bytes_per_second().unwrap() - 100000.0).abs() < 1.0);
        assert_eq!(p.budget(), 10000);
        p.observe(1, 6000, 495000, 10000, true);
        assert!(p.budget() < 4000);
    }
    #[test]
    fn undersupply_grows_fast_and_clamps() {
        let mut p = BulkPacer::new(1, false, 16384);
        p.observe(1, 0, 0, 0, true);
        let initial = p.budget();
        p.observe(1, 50, 32768, 0, true);
        assert_eq!(p.budget(), initial * 3 / 2);
        for i in 2..30 {
            p.observe(1, i * 50, i * 32768, 0, true);
        }
        assert_eq!(p.budget(), MAX_BULK_BUDGET);
        p.observe(1, 3000, 29 * 32768, 32768, true);
        assert_eq!(p.budget(), 32768);
    }
    #[test]
    fn generation_and_counter_rollback_reset_idle_does_not_decay() {
        let mut p = BulkPacer::new(100, true, 1000);
        p.observe(1, 0, 0, 10000, true);
        p.observe(1, 100, 5000, 10000, true);
        let rate = p.rate;
        p.observe(1, 10000, 5000, 0, false);
        p.observe(1, 20000, 5000, 0, true);
        assert_eq!(p.rate, rate);
        p.observe(2, 20050, 0, 0, true);
        assert_eq!(p.rate, None);
        assert_eq!(p.budget(), NATIVE_MIN_BUDGET);
        p.observe(2, 20150, 5000, 1000, true);
        p.observe(2, 20250, 0, 0, true);
        assert_eq!(p.rate, None);
    }
    #[test]
    fn native_rtt_minimum_and_frame_clamps() {
        let mut p = BulkPacer::new(100, true, 1000);
        p.note_rtt(400.0);
        p.note_rtt(300.0);
        p.note_rtt(600.0);
        p.observe(1, 0, 0, 10000, true);
        p.observe(1, 300, 600000, 10000, true);
        assert_eq!(p.budget(), 800000);
        p.note_frame(500000);
        assert_eq!(p.budget(), 1000000);
        let mut fallback = BulkPacer::new(100, true, 1000);
        fallback.observe(1, 0, 0, 10000, true);
        fallback.observe(1, 100, 200000, 10000, true);
        assert_eq!(fallback.budget(), 400000);
    }
    #[test]
    fn native_spans_ack_rounds_and_keeps_recent_delivery_peak() {
        let mut p = BulkPacer::new(50, true, INITIAL_MAX_FRAME);
        p.note_rtt(150.0);
        p.observe(1, 0, 0, 200000, true);
        p.observe(1, 50, 150000, 200000, true);
        assert_eq!(p.rate, None);
        p.observe(1, 150, 300000, 200000, true);
        assert_eq!(p.rate, Some(2000000.0));
        assert_eq!(p.budget(), 400000);
        // A budget-limited/ACK-quiet interval must not erase capacity.
        p.observe(1, 300, 300000, 200000, true);
        assert_eq!(p.rate, Some(2000000.0));
        assert_eq!(p.budget(), 400000);
        // Sustained stalls expire the peak and return to the native floor.
        p.observe(1, 2150, 300000, 200000, true);
        assert_eq!(p.rate, Some(0.0));
        assert_eq!(p.budget(), NATIVE_MIN_BUDGET);
        p.observe(2, 2300, 0, 200000, true);
        assert!(p.delivery_rates.is_empty());
        assert_eq!(p.rate, None);
    }
    #[test]
    fn native_probes_full_allowance_only_with_current_delivery() {
        let mut p = BulkPacer::new(50, true, INITIAL_MAX_FRAME);
        p.observe(1, 0, 0, 64000, true);
        p.observe(1, 100, 50000, 64000, true);
        assert_eq!(p.budget(), NATIVE_MIN_BUDGET * 3 / 2);
        let grown = p.budget();
        // A retained high rate cannot trigger a probe with no ACK progress.
        p.observe(1, 200, 50000, 64000, true);
        assert!(p.budget() < grown);
        assert_eq!(p.rate, Some(500000.0));
    }
    #[test]
    fn native_floor_bounds_slow_flight_and_preserves_frame_guard() {
        let mut p = BulkPacer::new(50, true, INITIAL_MAX_FRAME);
        assert_eq!(p.budget(), 64 * 1024);
        p.note_rtt(300.0);
        p.observe(1, 0, 0, 50000, true);
        // A slow link's BDP fits below the floor; do not retain the old 256 KiB.
        p.observe(1, 300, 30000, 50000, true);
        assert_eq!(p.budget(), 64 * 1024);
        p.note_frame(40000);
        assert_eq!(p.budget(), 80000);
        p.note_frame(MAX_BULK_BUDGET as usize);
        assert_eq!(p.budget(), MAX_BULK_BUDGET);
    }
    #[test]
    fn native_floor_survives_budget_limited_nonempty_flight() {
        let mut p = BulkPacer::new(50, true, INITIAL_MAX_FRAME);
        p.note_rtt(101.0);
        for i in 0..100 {
            p.observe(1, i * 150, i * 33084, 33084, true);
            assert_eq!(p.budget(), NATIVE_MIN_BUDGET);
        }
        assert!(p.rate_bytes_per_second().unwrap() < 250000.0);
    }
}
