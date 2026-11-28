//! SCTP adapter for the BBRv3 model: per-TSN transmission records, SACK
//! (cumulative + gap blocks) and loss notifications, RTO recovery and a pacer.
//!
//! Units are DATA chunk payload bytes, the unit of SCTP's own cwnd/rwnd and
//! inflight accounting; packet, chunk and DTLS headers are excluded.
//!
//! BBR in-flight differs from the inflight queue's byte count: a chunk
//! declared lost (fast-retransmit miss indication or T3-rtx) leaves BBR's
//! in-flight until it is retransmitted, as for a lost QUIC packet.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::bbr3::Bbr3;
use crate::util::sna32gte;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tsn {
    /// Outstanding transmission `id` (first or retransmission).
    InFlight { id: u64, bytes: u64 },
    /// Declared lost and not yet retransmitted.
    Lost { bytes: u64 },
}

/// quinn's explicit-rate pacer as patched for the QUIC probe
/// (tests/quicprobe/patches/README.md): one wakeup aims at a quantum
/// `q = clamp(rate / 500, 2 MSS, 64 MSS)` (2 ms of sending), the bucket holds
/// `2q` (one extra quantum absorbs timer overshoot), and elapsed time is
/// credited at the previous rate. The burst after idle is bounded by `2q`.
#[derive(Debug)]
struct Pacer {
    tokens: u64,
    capacity: u64,
    prev: Instant,
    rate: u64,
    fractional: f64,
}

fn quantum(rate: u64, mss: u64) -> u64 {
    (rate / 500).clamp(2 * mss, 64 * mss)
}

impl Pacer {
    fn new(now: Instant, rate: u64, mss: u64) -> Self {
        let capacity = 2 * quantum(rate.max(1), mss);
        Self {
            tokens: capacity,
            capacity,
            prev: now,
            rate: rate.max(1),
            fractional: 0.0,
        }
    }

    /// None when `bytes` may be sent now, else the time to wake up.
    fn delay(&mut self, now: Instant, rate: u64, bytes: u64, mss: u64) -> Option<Instant> {
        let rate = rate.max(1);
        let quantum = quantum(rate, mss);
        self.capacity = 2 * quantum;
        let elapsed = now.saturating_duration_since(self.prev).as_secs_f64();
        let credit = elapsed * self.rate as f64 + self.fractional;
        self.tokens = self.tokens.saturating_add(credit as u64).min(self.capacity);
        self.fractional = if self.tokens == self.capacity {
            0.0
        } else {
            credit.fract()
        };
        self.prev = now;
        self.rate = rate;
        if self.tokens >= bytes {
            return None;
        }
        let missing = (bytes.max(quantum) - self.tokens) as f64 - self.fractional;
        Some(now + Duration::from_secs_f64(missing / rate as f64).max(Duration::from_nanos(1)))
    }

    fn on_transmit(&mut self, bytes: u64) {
        self.tokens = self.tokens.saturating_sub(bytes)
    }
}

#[derive(Debug)]
pub(crate) struct SctpBbr {
    pub(crate) bbr: Bbr3,
    mss: u64,
    tsns: HashMap<u32, Tsn>,
    next_id: u64,
    inflight: u64,
    lost_pending: u64,
    /// RTO recovery ends once the cumulative TSN ack reaches this TSN.
    recovery_point: Option<u32>,
    pacer: Pacer,
    pub(crate) retransmissions: u64,
    pub(crate) app_limited_sends: u64,
}

impl SctpBbr {
    pub(crate) fn new(now: Instant, mss: u64) -> Self {
        let bbr = Bbr3::new(now, mss);
        let pacer = Pacer::new(now, bbr.pacing_rate(), mss);
        Self {
            bbr,
            mss,
            tsns: HashMap::new(),
            next_id: 0,
            inflight: 0,
            lost_pending: 0,
            recovery_point: None,
            pacer,
            retransmissions: 0,
            app_limited_sends: 0,
        }
    }

    pub(crate) fn cwnd(&self) -> u64 {
        self.bbr.cwnd()
    }

    pub(crate) fn inflight(&self) -> u64 {
        self.inflight
    }

    pub(crate) fn lost_pending(&self) -> u64 {
        self.lost_pending
    }

    /// Room for new transmissions under the BBR cwnd.
    pub(crate) fn window_allows(&self, bytes: u64) -> bool {
        self.inflight + bytes <= self.bbr.cwnd()
    }

    /// Pacing admission: Ok consumes tokens, Err gives the wakeup time.
    pub(crate) fn pace(&mut self, now: Instant, bytes: u64) -> Result<(), Instant> {
        match self
            .pacer
            .delay(now, self.bbr.pacing_rate(), bytes, self.mss)
        {
            None => {
                self.pacer.on_transmit(bytes);
                Ok(())
            }
            Some(at) => Err(at),
        }
    }

    /// Consume tokens for a transmission that is not gated (fast retransmit,
    /// zero-window probe), so the following new data is paced behind it.
    pub(crate) fn charge(&mut self, now: Instant, bytes: u64) {
        let _ = self
            .pacer
            .delay(now, self.bbr.pacing_rate(), bytes, self.mss);
        self.pacer.on_transmit(bytes);
    }

    fn transmit(&mut self, now: Instant, tsn: u32, bytes: u64, app_limited: bool, rtx: bool) {
        let id = self.next_id;
        self.next_id += 1;
        self.bbr
            .on_sent(now, id, bytes, self.inflight, app_limited, rtx);
        self.inflight += bytes;
        self.tsns.insert(tsn, Tsn::InFlight { id, bytes });
    }

    /// First transmission of `tsn`. `more_pending`: data remains queued after
    /// this chunk. The send is application-limited (delivery-rate draft §3.3)
    /// when nothing remains queued, it does not fill the cwnd and no lost data
    /// awaits retransmission.
    pub(crate) fn on_sent(&mut self, now: Instant, tsn: u32, bytes: u64, more_pending: bool) {
        let app_limited =
            !more_pending && self.lost_pending == 0 && self.inflight + bytes < self.bbr.cwnd();
        self.app_limited_sends += u64::from(app_limited);
        self.transmit(now, tsn, bytes, app_limited, false)
    }

    /// Retransmission (fast or T3-rtx) of `tsn`. It is in flight again but
    /// ambiguous: its SACK yields delivered bytes, no RTT or rate sample.
    pub(crate) fn on_retransmit(&mut self, now: Instant, tsn: u32) {
        if matches!(self.tsns.get(&tsn), Some(Tsn::InFlight { .. })) {
            // SCTP resends without a prior loss notification: count it lost.
            self.on_lost(now, tsn);
        }
        if let Some(Tsn::Lost { bytes }) = self.tsns.get(&tsn).copied() {
            self.lost_pending -= bytes;
            self.retransmissions += 1;
            self.transmit(now, tsn, bytes, false, true);
        }
    }

    /// `tsn` newly acknowledged by the cumulative TSN ack or a gap block.
    pub(crate) fn on_acked(&mut self, now: Instant, tsn: u32) {
        match self.tsns.remove(&tsn) {
            Some(Tsn::InFlight { id, bytes }) => {
                self.inflight -= bytes;
                self.bbr.on_acked(now, id);
            }
            Some(Tsn::Lost { bytes }) => {
                // Spurious loss: the earlier transmission arrived. No undo.
                self.lost_pending -= bytes;
                self.bbr.on_delivered_untracked(now, bytes);
            }
            None => {}
        }
    }

    /// `tsn` declared lost (third miss indication, or T3-rtx expiry).
    pub(crate) fn on_lost(&mut self, now: Instant, tsn: u32) {
        if let Some(Tsn::InFlight { id, bytes }) = self.tsns.get(&tsn).copied() {
            self.inflight -= bytes;
            self.lost_pending += bytes;
            self.tsns.insert(tsn, Tsn::Lost { bytes });
            self.bbr.on_lost(now, id, u64::from(tsn));
        }
    }

    /// End of one SACK: after its acks and the losses it revealed.
    pub(crate) fn on_sack_end(&mut self, now: Instant, cumulative_tsn_ack: u32) {
        if let Some(point) = self.recovery_point {
            if sna32gte(cumulative_tsn_ack, point) {
                self.recovery_point = None;
                self.bbr.on_recovery_end();
            }
        }
        self.bbr.on_ack_end(now, self.inflight);
    }

    /// T3-rtx expiry, after the caller declared the outstanding chunks lost.
    /// `highest_tsn` is the highest TSN sent so far (the recovery point).
    pub(crate) fn on_rto(&mut self, highest_tsn: u32) {
        self.bbr.on_rto(self.inflight);
        self.recovery_point = Some(highest_tsn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSS: u64 = 1200;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn gap_acks_losses_and_retransmissions_keep_inflight_and_delivered_consistent() {
        let t = Instant::now();
        let mut s = SctpBbr::new(t, MSS);
        let base = u32::MAX - 2; // exercise TSN wrap-around
        for i in 0..5u32 {
            s.on_sent(t, base.wrapping_add(i), 1000, true);
        }
        assert_eq!(s.inflight(), 5000);
        // SACK 1: cum ack = base, gap block acks base+2.
        s.on_acked(t + ms(50), base);
        s.on_acked(t + ms(50), base.wrapping_add(2));
        s.on_sack_end(t + ms(50), base);
        assert_eq!((s.inflight(), s.bbr.delivered()), (3000, 2000));
        assert_eq!(s.bbr.min_rtt(), Some(ms(50)));
        // A repeated gap report and an unknown TSN change nothing.
        s.on_acked(t + ms(60), base.wrapping_add(2));
        s.on_acked(t + ms(60), 12345);
        assert_eq!((s.inflight(), s.bbr.delivered()), (3000, 2000));
        // base+1 reaches three miss indications.
        s.on_lost(t + ms(70), base.wrapping_add(1));
        s.on_lost(t + ms(70), base.wrapping_add(1));
        assert_eq!(
            (s.inflight(), s.lost_pending(), s.bbr.lost()),
            (2000, 1000, 1000)
        );
        // Fast retransmit puts it back in flight, ambiguous.
        s.on_retransmit(t + ms(70), base.wrapping_add(1));
        assert_eq!(
            (s.inflight(), s.lost_pending(), s.retransmissions),
            (3000, 0, 1)
        );
        // A SACK that only covers the retransmission delivers bytes but no
        // RTT sample (the 1 ms here would otherwise lower min RTT).
        s.on_acked(t + ms(71), base.wrapping_add(1));
        s.on_sack_end(t + ms(71), base.wrapping_add(2));
        assert_eq!((s.inflight(), s.bbr.delivered()), (2000, 3000));
        assert_eq!(s.bbr.min_rtt(), Some(ms(50)));
    }

    #[test]
    fn spurious_loss_is_delivered_once_without_undo() {
        let t = Instant::now();
        let mut s = SctpBbr::new(t, MSS);
        s.on_sent(t, 7, 1000, true);
        s.on_lost(t + ms(10), 7);
        s.on_acked(t + ms(20), 7);
        assert_eq!((s.inflight(), s.lost_pending()), (0, 0));
        assert_eq!((s.bbr.delivered(), s.bbr.lost()), (1000, 1000));
        // Later retransmission attempts of an acked TSN are ignored.
        s.on_retransmit(t + ms(30), 7);
        assert_eq!((s.inflight(), s.retransmissions), (0, 0));
    }

    #[test]
    fn retransmission_without_loss_notice_counts_the_loss_first() {
        let t = Instant::now();
        let mut s = SctpBbr::new(t, MSS);
        s.on_sent(t, 1, 800, true);
        s.on_retransmit(t + ms(5), 1);
        assert_eq!(
            (s.inflight(), s.lost_pending(), s.bbr.lost()),
            (800, 0, 800)
        );
        // The retransmission itself can be lost again (e.g. by T3-rtx).
        s.on_lost(t + ms(10), 1);
        assert_eq!(
            (s.inflight(), s.lost_pending(), s.bbr.lost()),
            (0, 800, 1600)
        );
    }

    #[test]
    fn rto_recovery_collapses_then_restores_at_the_recovery_point() {
        let t = Instant::now();
        let mut s = SctpBbr::new(t, MSS);
        for tsn in 1..=10u32 {
            s.on_sent(t, tsn, 1200, true);
        }
        let before = s.cwnd();
        for tsn in 1..=10u32 {
            s.on_lost(t + ms(1000), tsn);
        }
        s.on_rto(10);
        assert_eq!(s.cwnd(), MSS);
        assert!(s.bbr.in_recovery());
        s.on_retransmit(t + ms(1000), 1);
        assert!(!s.window_allows(1));
        s.on_acked(t + ms(1100), 1);
        s.on_sack_end(t + ms(1100), 1);
        assert!(s.bbr.in_recovery());
        assert!(s.cwnd() >= 4 * MSS);
        for tsn in 2..=10u32 {
            s.on_retransmit(t + ms(1100), tsn);
            s.on_acked(t + ms(1200), tsn);
        }
        s.on_sack_end(t + ms(1200), 10);
        assert!(!s.bbr.in_recovery());
        assert!(s.cwnd() >= before);
    }

    #[test]
    fn app_limited_only_when_idle_window_and_no_repair_pending() {
        let t = Instant::now();
        let mut s = SctpBbr::new(t, MSS);
        // Nothing queued after this chunk and far below cwnd: app-limited.
        // An app-limited send from idle restarts ACK aggregation (no panic,
        // no state change in Startup).
        s.on_sent(t, 1, 100, false);
        s.on_acked(t + ms(10), 1);
        s.on_sack_end(t + ms(10), 1);
        assert_eq!(s.bbr.state, crate::congestion::bbr3::State::Startup);
        assert_eq!(s.app_limited_sends, 1);
        // More queued data, then a pending repair, suppress the mark.
        s.on_sent(t + ms(20), 2, 100, true);
        s.on_lost(t + ms(30), 2);
        s.on_sent(t + ms(30), 3, 100, false);
        assert_eq!((s.lost_pending(), s.app_limited_sends), (100, 1));
        // Filling the window is cwnd-limited, not app-limited.
        s.on_retransmit(t + ms(30), 2);
        let fill = s.cwnd() - s.inflight();
        s.on_sent(t + ms(31), 4, fill, false);
        assert_eq!(s.app_limited_sends, 1);
    }

    #[test]
    fn pacer_bounds_bursts_and_schedules_the_next_quantum() {
        let t = Instant::now();
        let rate = 1_000_000; // 1 MB/s: quantum 2000 -> 2400 (2 MSS), bucket 4800
        let mut p = Pacer::new(t, rate, MSS);
        let mut sent = 0;
        while p.delay(t, rate, MSS, MSS).is_none() {
            p.on_transmit(MSS);
            sent += MSS;
        }
        assert_eq!(sent, 4 * MSS);
        // The wakeup waits for a full quantum: 2400 B at 1 MB/s = 2.4 ms.
        let at = p.delay(t, rate, MSS, MSS).unwrap();
        let wait = (at - t).as_nanos() as i128;
        assert!((wait - 2_400_000).abs() <= 1, "{wait}");
        assert!(p.delay(at, rate, MSS, MSS).is_none());
        // A long idle period refills only the bucket.
        p.on_transmit(MSS);
        let later = at + Duration::from_secs(10);
        let mut burst = 0;
        while p.delay(later, rate, MSS, MSS).is_none() {
            p.on_transmit(MSS);
            burst += MSS;
        }
        assert_eq!(burst, 4 * MSS);
    }

    #[test]
    fn pace_and_charge_share_the_bucket() {
        let t = Instant::now();
        let mut s = SctpBbr::new(t, MSS);
        let mut n = 0;
        while s.pace(t, MSS).is_ok() {
            n += 1;
        }
        assert!(n >= 4);
        let blocked = s.pace(t, MSS).unwrap_err();
        assert!(blocked > t);
        // A charged (ungated) send pushes the next admission further out.
        let mut s2 = SctpBbr::new(t, MSS);
        s2.charge(t, 100 * MSS);
        assert!(s2.pace(t, MSS).is_err());
    }
}
