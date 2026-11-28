//! Per-ACK delivery samples, draft-cheng-iccrg-delivery-rate-estimation-02
//! §3 and draft-ietf-ccwg-bbr-06 §4.1. Ported from tests/quicprobe/src/bbr3.
//!
//! SCTP retransmits under the original TSN, so a SACK for a retransmitted
//! chunk cannot be attributed to one transmission. Such transmissions are
//! `ambiguous`: their bytes count as delivered (C.delivered, C.delivered_time)
//! but they never become the sample's packet, so they produce neither an RTT
//! nor a delivery-rate sample (Karn's rule applied to both).
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Packet {
    pub(crate) sent: Instant,
    first_sent: Instant,
    delivered_time: Instant,
    pub(crate) prior_delivered: u64,
    pub(crate) bytes: u64,
    pub(crate) tx_in_flight: u64,
    pub(crate) lost_at_send: u64,
    pub(crate) app_limited: bool,
    pub(crate) probe: Option<u64>,
    ambiguous: bool,
}

#[derive(Debug)]
pub(crate) struct Sample {
    pub(crate) packet: Packet,
    pub(crate) acked: u64,
    pub(crate) delivered: u64,
    pub(crate) rate: Option<u64>,
    pub(crate) rtt: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct Sampler {
    outstanding: BTreeMap<u64, Packet>,
    pub(crate) delivered: u64,
    pub(crate) lost: u64,
    first_sent: Instant,
    delivered_time: Instant,
    limited_until: Option<u64>,
    newest: Option<(u64, Packet)>,
    acked: u64,
}

impl Sampler {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            outstanding: BTreeMap::new(),
            delivered: 0,
            lost: 0,
            first_sent: now,
            delivered_time: now,
            limited_until: None,
            newest: None,
            acked: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn sent(
        &mut self,
        now: Instant,
        id: u64,
        bytes: u64,
        inflight: u64,
        limited: bool,
        probe: Option<u64>,
        ambiguous: bool,
    ) {
        if inflight == 0 {
            self.first_sent = now;
            self.delivered_time = now;
        }
        if limited {
            self.limited_until = Some(self.delivered + inflight);
        }
        self.outstanding.insert(
            id,
            Packet {
                sent: now,
                first_sent: self.first_sent,
                delivered_time: self.delivered_time,
                prior_delivered: self.delivered,
                bytes,
                tx_in_flight: inflight + bytes,
                lost_at_send: self.lost,
                app_limited: self.limited_until.is_some(),
                probe,
                ambiguous,
            },
        );
    }

    pub(crate) fn ack(&mut self, now: Instant, id: u64) {
        let Some(packet) = self.outstanding.remove(&id) else {
            return;
        };
        self.deliver(now, packet.bytes);
        if packet.ambiguous {
            return;
        }
        if self
            .newest
            .is_none_or(|(prev, p)| (packet.sent, id) > (p.sent, prev))
        {
            self.newest = Some((id, packet));
        }
    }

    /// Delivery of bytes with no outstanding transmission record, e.g. a chunk
    /// already declared lost whose original transmission is acknowledged late.
    pub(crate) fn deliver(&mut self, now: Instant, bytes: u64) {
        self.delivered += bytes;
        self.acked += bytes;
        self.delivered_time = now;
    }

    /// Newly acked bytes of an ACK that produced no sample.
    pub(crate) fn take_acked(&mut self) -> u64 {
        std::mem::take(&mut self.acked)
    }

    pub(crate) fn finish(&mut self, now: Instant, min_rtt: Duration) -> Option<Sample> {
        let (_, packet) = self.newest.take()?;
        self.first_sent = self.first_sent.max(packet.sent);
        if self.limited_until.is_some_and(|end| self.delivered > end) {
            self.limited_until = None;
        }
        let interval = (packet.sent - packet.first_sent).max(now - packet.delivered_time);
        let delivered = self.delivered - packet.prior_delivered;
        let rate = (interval >= min_rtt && !interval.is_zero())
            .then(|| (delivered as f64 / interval.as_secs_f64()) as u64);
        Some(Sample {
            packet,
            acked: std::mem::take(&mut self.acked),
            delivered,
            rate,
            rtt: now - packet.sent,
        })
    }

    pub(crate) fn lose(&mut self, id: u64) -> Option<Packet> {
        let packet = self.outstanding.remove(&id)?;
        self.lost += packet.bytes;
        Some(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batched_ack_uses_newest_send_and_slower_interval() {
        let t = Instant::now();
        let ms = Duration::from_millis;
        let mut s = Sampler::new(t);
        s.sent(t, 1, 1000, 0, false, None, false);
        s.sent(t + ms(10), 2, 1000, 1000, false, None, false);
        s.ack(t + ms(100), 1);
        assert_eq!(s.finish(t + ms(100), ms(90)).unwrap().rate, Some(10_000));
        s.sent(t + ms(190), 3, 1000, 1000, false, None, false);
        s.ack(t + ms(200), 3);
        s.ack(t + ms(200), 2);
        s.ack(t + ms(200), 3);
        let batch = s.finish(t + ms(200), ms(90)).unwrap();
        assert_eq!(batch.acked, 2000);
        assert_eq!(batch.rate, Some(10_526)); // 2000 B / max(190ms, 100ms)
        assert_eq!(batch.rtt, ms(10));
    }

    #[test]
    fn loss_idle_restart_and_short_sample() {
        let t = Instant::now();
        let mut s = Sampler::new(t);
        s.sent(t, 1, 900, 0, false, None, false);
        assert!(s.lose(1).is_some());
        s.ack(t, 1);
        assert!(s.finish(t, Duration::ZERO).is_none());
        let restart = t + Duration::from_secs(5);
        s.sent(restart, 2, 1000, 0, true, None, false);
        s.ack(restart + Duration::from_millis(10), 2);
        let sample = s
            .finish(
                restart + Duration::from_millis(10),
                Duration::from_millis(20),
            )
            .unwrap();
        assert!(sample.packet.app_limited);
        assert_eq!(sample.rate, None);
        assert_eq!(sample.packet.lost_at_send, 900);
    }

    #[test]
    fn ambiguous_retransmission_delivers_without_a_sample() {
        let t = Instant::now();
        let ms = Duration::from_millis;
        let mut s = Sampler::new(t);
        s.sent(t, 1, 1000, 0, false, None, false);
        s.sent(t + ms(5), 2, 1000, 1000, false, None, true);
        s.ack(t + ms(50), 2);
        assert_eq!(s.delivered, 1000);
        assert!(s.finish(t + ms(50), Duration::ZERO).is_none());
        // The credit carries into the next unambiguous sample.
        s.ack(t + ms(100), 1);
        let sample = s.finish(t + ms(100), Duration::ZERO).unwrap();
        assert_eq!(sample.acked, 2000);
        assert_eq!(sample.rtt, ms(100));
        s.deliver(t + ms(120), 500);
        assert_eq!(s.delivered, 2500);
    }
}
