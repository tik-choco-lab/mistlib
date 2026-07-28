//! Ported from mistlink/internal/receiver/stats.go. Logging is kept as a
//! side effect (via `tracing`), matching the Go original's `logIfTime`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::media::rtp::nal;

const LOG_INTERVAL: Duration = Duration::from_secs(5);
const GAP_WARNING_THRESHOLD: i32 = 10;

/// Per-track packet/gap/NAL-type statistics, periodically logged.
pub struct TrackStats {
    packet_count: u32,
    last_log_time: Instant,
    nal_type_stats: HashMap<u8, u32>,
    last_sequence_number: u16,
    sequence_gaps: i32,
    first_packet: bool,
}

impl Default for TrackStats {
    fn default() -> Self {
        Self {
            packet_count: 0,
            last_log_time: Instant::now(),
            nal_type_stats: HashMap::new(),
            last_sequence_number: 0,
            sequence_gaps: 0,
            first_packet: true,
        }
    }
}

impl TrackStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the sequence numbers skipped since the last packet. Unlike
    /// `stream::TrackBroadcaster`'s gap check, this has no upper bound on gap
    /// size (matching the Go original) and tracks a running gap counter that
    /// decrements on out-of-order (negative-diff) packets.
    pub fn check_sequence_gap(&mut self, current_seq: u16) -> Vec<u16> {
        if self.first_packet {
            self.last_sequence_number = current_seq;
            self.first_packet = false;
            return Vec::new();
        }

        let diff = current_seq.wrapping_sub(self.last_sequence_number) as i16;
        let mut missing = Vec::new();

        match diff {
            1 => self.last_sequence_number = current_seq,
            0 => {}
            d if d > 1 => {
                let gap = (d - 1) as i32;
                self.sequence_gaps += gap;
                for i in 1..=gap as u16 {
                    missing.push(self.last_sequence_number.wrapping_add(i));
                }
                if gap > GAP_WARNING_THRESHOLD {
                    tracing::warn!(
                        "sequence gap: {} -> {} (lost: {gap})",
                        self.last_sequence_number,
                        current_seq
                    );
                }
                self.last_sequence_number = current_seq;
            }
            _ => {
                if self.sequence_gaps > 0 {
                    self.sequence_gaps -= 1;
                }
            }
        }

        missing
    }

    pub fn update_nal_stats(&mut self, nal_type: u8, has_idr: bool) {
        if nal_type != 0 {
            *self.nal_type_stats.entry(nal_type).or_insert(0) += 1;
        }
        if has_idr {
            *self.nal_type_stats.entry(nal::NAL_TYPE_IDR).or_insert(0) += 1;
        }
    }

    pub fn record_packet(&mut self) {
        self.packet_count += 1;
    }

    /// Logs and resets accumulated stats if `LOG_INTERVAL` has elapsed since
    /// the last log.
    pub fn log_if_time(&mut self, is_video: bool) {
        if self.last_log_time.elapsed() <= LOG_INTERVAL {
            return;
        }

        let expected = self.packet_count as i32 + self.sequence_gaps;
        let loss_rate = if expected > 0 && self.sequence_gaps > 0 {
            Some(self.sequence_gaps as f64 / expected as f64 * 100.0)
        } else {
            None
        };

        if is_video && !self.nal_type_stats.is_empty() {
            let stats_str: Vec<String> = self
                .nal_type_stats
                .iter()
                .map(|(nal_type, count)| {
                    format!("{}({nal_type})={count}", nal::get_nal_type_name(*nal_type))
                })
                .collect();
            let mut line = format!("VIDEO NAL stats: {}", stats_str.join(", "));
            if let Some(rate) = loss_rate {
                line.push_str(&format!(" | loss: {} ({rate:.2}%)", self.sequence_gaps));
            }
            tracing::debug!("{line}");
            self.nal_type_stats.clear();
        } else if !is_video {
            let mut line = format!("AUDIO packets: {}", self.packet_count);
            if let Some(rate) = loss_rate {
                line.push_str(&format!(" | loss: {} ({rate:.2}%)", self.sequence_gaps));
            }
            tracing::debug!("{line}");
        }

        self.sequence_gaps = 0;
        self.packet_count = 0;
        self.last_log_time = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_packet_has_no_gap() {
        let mut stats = TrackStats::new();
        assert!(stats.check_sequence_gap(100).is_empty());
    }

    #[test]
    fn sequential_packets_have_no_gap() {
        let mut stats = TrackStats::new();
        stats.check_sequence_gap(1);
        assert!(stats.check_sequence_gap(2).is_empty());
    }

    #[test]
    fn gap_reports_missing_sequence_numbers_and_accumulates() {
        let mut stats = TrackStats::new();
        stats.check_sequence_gap(10);
        let missing = stats.check_sequence_gap(13);
        assert_eq!(missing, vec![11, 12]);
        assert_eq!(stats.sequence_gaps, 2);
    }

    #[test]
    fn out_of_order_packet_decrements_gap_counter() {
        let mut stats = TrackStats::new();
        stats.check_sequence_gap(10);
        stats.check_sequence_gap(13);
        assert_eq!(stats.sequence_gaps, 2);
        stats.check_sequence_gap(12);
        assert_eq!(stats.sequence_gaps, 1);
    }

    #[test]
    fn update_nal_stats_counts_idr_separately_from_fu_a() {
        let mut stats = TrackStats::new();
        stats.update_nal_stats(nal::NAL_TYPE_FU_A, true);
        assert_eq!(stats.nal_type_stats.get(&nal::NAL_TYPE_FU_A), Some(&1));
        assert_eq!(stats.nal_type_stats.get(&nal::NAL_TYPE_IDR), Some(&1));
    }
}
