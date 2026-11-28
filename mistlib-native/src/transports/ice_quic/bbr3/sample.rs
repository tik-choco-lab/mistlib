//! Per-ACK delivery samples, draft-cheng-iccrg-delivery-rate-estimation-02
//! §3 and draft-ietf-ccwg-bbr-06 §4.1. Sizes include QUIC framing.
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(super) struct Packet {
    pub sent: Instant,
    first_sent: Instant,
    delivered_time: Instant,
    pub prior_delivered: u64,
    pub bytes: u16,
    pub tx_in_flight: u64,
    pub lost_at_send: u64,
    pub app_limited: bool,
    pub probe: Option<u64>,
}

pub(super) struct Sample {
    pub packet: Packet,
    pub acked: u64,
    pub delivered: u64,
    pub rate: Option<u64>,
    pub rtt: Duration,
}

#[derive(Clone)]
pub(super) struct Sampler {
    outstanding: BTreeMap<u64, Packet>,
    pub delivered: u64,
    pub lost: u64,
    first_sent: Instant,
    delivered_time: Instant,
    limited_until: Option<u64>,
    newest: Option<(u64, Packet)>,
    acked: u64,
}

impl Sampler {
    pub fn new(now: Instant) -> Self {
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

    pub fn sent(
        &mut self,
        now: Instant,
        pn: u64,
        bytes: u16,
        inflight: u64,
        limited: bool,
        probe: Option<u64>,
    ) {
        if inflight == 0 {
            self.first_sent = now;
            self.delivered_time = now;
        }
        if limited {
            self.limited_until = Some(self.delivered + inflight);
        }
        self.outstanding.insert(
            pn,
            Packet {
                sent: now,
                first_sent: self.first_sent,
                delivered_time: self.delivered_time,
                prior_delivered: self.delivered,
                bytes,
                tx_in_flight: inflight + u64::from(bytes),
                lost_at_send: self.lost,
                app_limited: self.limited_until.is_some(),
                probe,
            },
        );
    }

    pub fn ack(&mut self, now: Instant, pn: u64) {
        let Some(packet) = self.outstanding.remove(&pn) else {
            return;
        };
        self.delivered += u64::from(packet.bytes);
        self.acked += u64::from(packet.bytes);
        self.delivered_time = now;
        if self
            .newest
            .is_none_or(|(prev, p)| (packet.sent, pn) > (p.sent, prev))
        {
            self.newest = Some((pn, packet));
        }
    }

    pub fn finish(&mut self, now: Instant, min_rtt: Duration) -> Option<Sample> {
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

    pub fn lose(&mut self, pn: u64) -> Option<Packet> {
        let packet = self.outstanding.remove(&pn)?;
        self.lost += u64::from(packet.bytes);
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
        s.sent(t, 1, 1000, 0, false, None);
        s.sent(t + ms(10), 2, 1000, 1000, false, None);
        s.ack(t + ms(100), 1);
        assert_eq!(s.finish(t + ms(100), ms(90)).unwrap().rate, Some(10_000));
        s.sent(t + ms(190), 3, 1000, 1000, false, None);
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
        s.sent(t, 1, 900, 0, false, None);
        assert!(s.lose(1).is_some());
        s.ack(t, 1);
        assert!(s.finish(t, Duration::ZERO).is_none());
        let restart = t + Duration::from_secs(5);
        s.sent(restart, 2, 1000, 0, true, None);
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
}
