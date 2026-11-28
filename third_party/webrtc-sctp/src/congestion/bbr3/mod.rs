//! Transport-independent, single-path research implementation of
//! draft-ietf-ccwg-bbr-06 with draft-cheng-iccrg-delivery-rate-estimation-02
//! sampling. This is the model of tests/quicprobe/src/bbr3 (driven there by
//! quinn's Controller trait) with the quinn types removed, so that SCTP and
//! QUIC can be compared under the same controller. Differences from the probe
//! copy are limited to: byte sizes are `u64`, retransmissions can be marked
//! ambiguous (see `sample`), and the draft's RTO response (BBROnEnterRTO /
//! BBRRestoreCwnd) is available because SCTP's T3-rtx declares every
//! outstanding chunk lost. See MISTLIB_PATCHES.md for the remaining omissions.
mod filter;
mod sample;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use rand::Rng;

use filter::{MaxBw, MinRtt};
use sample::{Packet, Sample, Sampler};

/// Initial pacing assumes quinn's initial RTT estimate, as in the QUIC probe.
pub(crate) const INITIAL_RTT: Duration = Duration::from_millis(333);
const UNBOUNDED: u64 = u64::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    Startup,
    Drain,
    Down,
    Cruise,
    Refill,
    Up,
    ProbeRtt,
}

impl State {
    fn pacing_gain(self) -> f64 {
        match self {
            Self::Startup => 2.77,
            Self::Drain => 0.5,
            Self::Down => 0.9,
            Self::Up => 1.25,
            _ => 1.0,
        }
    }
    fn probing(self) -> bool {
        matches!(self, Self::Startup | Self::Refill | Self::Up)
    }
}

#[derive(Clone, Default, Debug)]
struct Plateau {
    baseline: u64,
    rounds: u8,
}

impl Plateau {
    fn observe(&mut self, rate: u64) -> bool {
        if rate as f64 >= self.baseline as f64 * 1.25 {
            self.baseline = rate;
            self.rounds = 0;
        } else {
            self.rounds = self.rounds.saturating_add(1);
        }
        self.rounds >= 3
    }
}

#[derive(Clone, Default, Debug)]
struct LossRound {
    end_delivered: u64,
    ranges: u32,
    last_seq: Option<u64>,
    excessive: bool,
    max_bw: u64,
    max_delivered: u64,
}

/// Compressed ACK credit, measured in bytes above the expected delivery rate.
/// Keep the largest excess within each of the last ten packet-timed rounds.
#[derive(Clone, Debug)]
struct AckAggregation {
    since: Instant,
    bytes: u64,
    peaks: VecDeque<(u64, u64)>,
}

impl AckAggregation {
    fn update(
        &mut self,
        now: Instant,
        acked: u64,
        bw: u64,
        cwnd: u64,
        round: u64,
        rounds: u64,
    ) -> u64 {
        let mut expected = volume(bw, now - self.since);
        if self.bytes <= expected {
            self.since = now;
            self.bytes = 0;
            expected = 0;
        }
        self.bytes += acked;
        let excess = (self.bytes - expected).min(cwnd);
        self.peaks.retain(|&(r, _)| round - r < rounds);
        match self.peaks.back_mut() {
            Some((r, peak)) if *r == round => *peak = (*peak).max(excess),
            _ => self.peaks.push_back((round, excess)),
        }
        self.peaks.iter().map(|&(_, peak)| peak).max().unwrap_or(0)
    }
}

fn volume(rate: u64, duration: Duration) -> u64 {
    (rate as f64 * duration.as_secs_f64()) as u64
}

pub(crate) fn pacing_rate(bw: u64, gain: f64) -> u64 {
    ((bw as f64 * gain * 0.99) as u64).max(1)
}

fn excessive_loss(lost: u64, in_flight: u64) -> bool {
    u128::from(lost) * 50 > u128::from(in_flight)
}

#[derive(Clone, Debug)]
pub(crate) struct Bbr3 {
    pub(crate) state: State,
    sampler: Sampler,
    max_bw: MaxBw,
    min_rtt: MinRtt,
    mtu: u64,
    initial_window: u64,
    cwnd: u64,
    pacing: u64,
    bw_lo: u64,
    inflight_lo: u64,
    inflight_hi: u64,
    aggregation: AckAggregation,
    round: u64,
    round_end: u64,
    state_round: u64,
    loss: LossRound,
    plateau: Plateau,
    filled: bool,
    probe_id: u64,
    probe_limited: bool,
    previous_probe_limited: bool,
    precautionary: bool,
    cycle_start_round: u64,
    probe_wait_rounds: u64,
    probe_after: Instant,
    advance_bw: bool,
    probe_up_acked: u64,
    prior_cwnd: u64,
    rtt_done: Option<(Instant, u64)>,
    idle_restart: bool,
    /// RTO recovery (BBROnEnterRTO until BBRRestoreCwnd).
    in_recovery: bool,
    pub(crate) high_loss_probes: u64,
    pub(crate) short_term_cuts: u64,
    pub(crate) rto_events: u64,
}

impl Bbr3 {
    /// `mtu` is the sender's maximum segment in the same units as the byte
    /// counts passed to `on_sent` (SCTP: DATA chunk payload bytes).
    pub(crate) fn new(now: Instant, mtu: u64) -> Self {
        let initial_window = (10 * mtu).min(14720.max(2 * mtu));
        let nominal_bw = (initial_window as f64 / INITIAL_RTT.as_secs_f64()) as u64;
        Self {
            state: State::Startup,
            sampler: Sampler::new(now),
            max_bw: MaxBw::default(),
            min_rtt: MinRtt::new(now),
            mtu,
            initial_window,
            cwnd: initial_window,
            pacing: pacing_rate(nominal_bw, State::Startup.pacing_gain()),
            bw_lo: UNBOUNDED,
            inflight_lo: UNBOUNDED,
            inflight_hi: UNBOUNDED,
            aggregation: AckAggregation {
                since: now,
                bytes: 0,
                peaks: VecDeque::new(),
            },
            round: 0,
            round_end: 0,
            state_round: 0,
            loss: LossRound::default(),
            plateau: Plateau::default(),
            filled: false,
            probe_id: 0,
            probe_limited: false,
            previous_probe_limited: false,
            precautionary: false,
            cycle_start_round: 0,
            probe_wait_rounds: 63,
            probe_after: now,
            advance_bw: false,
            probe_up_acked: 0,
            prior_cwnd: initial_window,
            rtt_done: None,
            idle_restart: false,
            in_recovery: false,
            high_loss_probes: 0,
            short_term_cuts: 0,
            rto_events: 0,
        }
    }

    pub(crate) fn cwnd(&self) -> u64 {
        self.cwnd
    }

    pub(crate) fn pacing_rate(&self) -> u64 {
        self.pacing
    }

    pub(crate) fn delivered(&self) -> u64 {
        self.sampler.delivered
    }

    pub(crate) fn lost(&self) -> u64 {
        self.sampler.lost
    }

    pub(crate) fn min_rtt(&self) -> Option<Duration> {
        (self.min_rtt.value != Duration::MAX).then_some(self.min_rtt.value)
    }

    pub(crate) fn bw(&self) -> u64 {
        self.max_bw.get().min(self.bw_lo)
    }

    fn bdp(&self) -> u64 {
        if self.min_rtt.value == Duration::MAX || self.bw() == 0 {
            return self.initial_window;
        }
        volume(self.bw(), self.min_rtt.value)
    }

    fn headroom(&self) -> u64 {
        if self.inflight_hi == UNBOUNDED {
            return UNBOUNDED;
        }
        (self.inflight_hi - self.inflight_hi.saturating_mul(15) / 100).max(4 * self.mtu)
    }

    fn probe_rtt_cwnd(&self) -> u64 {
        (self.bdp() / 2).max(4 * self.mtu)
    }

    /// BBRSaveCwnd.
    fn save_cwnd(&self) -> u64 {
        if !self.in_recovery && self.state != State::ProbeRtt {
            self.cwnd
        } else {
            self.prior_cwnd.max(self.cwnd)
        }
    }

    fn enter(&mut self, now: Instant, state: State) {
        if state == State::ProbeRtt {
            self.prior_cwnd = self.save_cwnd();
            self.rtt_done = None;
        }
        log::debug!(
            "bbr3 {:?} -> {:?}: bw={} min_rtt={:?} cwnd={} inflight_hi={} pacing={}",
            self.state,
            state,
            self.bw(),
            self.min_rtt(),
            self.cwnd,
            self.inflight_hi,
            self.pacing
        );
        self.state = state;
        self.state_round = self.round;
        match state {
            State::Down => {
                let mut rng = rand::thread_rng();
                self.cycle_start_round = self.round;
                self.probe_after = now + Duration::from_millis(rng.gen_range(2000..3000));
                self.probe_wait_rounds = rng.gen_range(62..=63);
                self.advance_bw = true;
                self.round_end = self.sampler.delivered;
            }
            State::Refill => {
                self.reset_short_term();
                self.probe_id += 1;
                self.previous_probe_limited = self.probe_limited;
                self.probe_limited = false;
                self.precautionary = false;
                self.round_end = self.sampler.delivered;
            }
            State::Up => {
                self.round_end = self.sampler.delivered;
                self.plateau = Plateau::default();
                self.probe_up_acked = 0;
            }
            _ => {}
        }
    }

    fn reset_short_term(&mut self) {
        self.inflight_lo = UNBOUNDED;
        self.bw_lo = UNBOUNDED;
        self.loss = LossRound::default();
    }

    fn update_loss_round(&mut self, s: &Sample) -> bool {
        self.loss.max_bw = self.loss.max_bw.max(s.rate.unwrap_or(0));
        self.loss.max_delivered = self.loss.max_delivered.max(s.delivered);
        if s.packet.prior_delivered < self.loss.end_delivered {
            return false;
        }
        let startup_loss =
            self.state == State::Startup && self.loss.ranges >= 6 && self.loss.excessive;
        if startup_loss {
            self.inflight_hi = self.bdp().max(self.loss.max_delivered);
        } else if !self.state.probing() && self.loss.ranges != 0 {
            let bw = self.bw_lo.min(self.max_bw.get());
            self.bw_lo = self.loss.max_bw.max(bw.saturating_mul(7) / 10);
            let inflight = if self.inflight_lo == UNBOUNDED {
                self.cwnd
            } else {
                self.inflight_lo
            };
            self.inflight_lo = self.loss.max_delivered.max(inflight.saturating_mul(7) / 10);
            self.short_term_cuts += 1;
        }
        self.loss = LossRound {
            end_delivered: self.sampler.delivered,
            max_bw: s.rate.unwrap_or(0),
            max_delivered: s.delivered,
            ..Default::default()
        };
        startup_loss
    }

    // Feedback is tagged at send time, so a loss detected after UP ends can
    // still constrain the probe that caused it, but only once per probe.
    fn check_probe_loss(&mut self, now: Instant, p: Packet, lost_packet: bool) {
        let lost = self.sampler.lost - p.lost_at_send;
        if p.probe != Some(self.probe_id)
            || self.probe_limited
            || !excessive_loss(lost, p.tx_in_flight)
        {
            return;
        }
        self.probe_limited = true;
        self.high_loss_probes += 1;
        if !p.app_limited {
            let mut at_loss = p.tx_in_flight;
            if lost_packet {
                let size = p.bytes;
                let previous = at_loss - size;
                let prefix = previous.saturating_sub((lost - size).saturating_mul(50)) / 49;
                at_loss = previous + prefix.min(size);
            }
            self.inflight_hi = at_loss.max(self.bdp().min(self.cwnd).saturating_mul(7) / 10);
        }
        if self.state == State::Up {
            self.enter(now, State::Down)
        }
    }

    fn update_state(
        &mut self,
        now: Instant,
        s: &Sample,
        in_flight: u64,
        round_start: bool,
        startup_loss: bool,
    ) {
        let full = matches!(self.state, State::Startup | State::Up)
            && round_start
            && !s.packet.app_limited
            && s.rate.is_some_and(|rate| self.plateau.observe(rate));
        let bdp = self.bdp().max(4 * self.mtu);
        if self.advance_bw && round_start && matches!(self.state, State::Down | State::Cruise) {
            if !s.packet.app_limited {
                self.max_bw.advance()
            }
            self.advance_bw = false;
            if self.precautionary && !self.probe_limited {
                self.enter(now, State::Refill);
                return;
            }
        }
        match self.state {
            State::Startup if full || startup_loss => {
                self.filled = true;
                self.enter(now, State::Drain);
            }
            State::Drain if in_flight <= bdp || self.round - self.state_round >= 3 => {
                self.enter(now, State::Down)
            }
            State::Down | State::Cruise => {
                let reno_rounds = self
                    .probe_wait_rounds
                    .min(self.bdp().min(self.cwnd).div_ceil(self.mtu))
                    .max(1);
                if now >= self.probe_after || self.round - self.cycle_start_round >= reno_rounds {
                    self.enter(now, State::Refill);
                } else if self.state == State::Down && in_flight <= bdp.min(self.headroom()) {
                    self.enter(now, State::Cruise);
                }
            }
            State::Refill if round_start => self.enter(now, State::Up),
            State::Up => {
                let limited =
                    s.packet.tx_in_flight + self.mtu >= self.cwnd && self.cwnd >= self.inflight_hi;
                if self.previous_probe_limited && s.packet.tx_in_flight >= self.inflight_hi {
                    self.precautionary = true;
                    self.enter(now, State::Down);
                } else if limited {
                    self.plateau = Plateau {
                        baseline: s.rate.unwrap_or(0),
                        rounds: 0,
                    };
                    let slope = 1 << (self.round - self.state_round).min(30);
                    let per_mtu = (self.cwnd / slope).max(self.mtu);
                    self.probe_up_acked += s.acked;
                    self.inflight_hi = self
                        .inflight_hi
                        .saturating_add((self.probe_up_acked / per_mtu) * self.mtu);
                    self.probe_up_acked %= per_mtu;
                } else if full {
                    self.enter(now, State::Down);
                }
            }
            _ => {}
        }
    }

    fn update_probe_rtt(&mut self, now: Instant, in_flight: u64, expired: bool) {
        if self.state != State::ProbeRtt && expired && !self.idle_restart {
            self.enter(now, State::ProbeRtt);
        }
        if self.state == State::ProbeRtt {
            if let Some((deadline, start_round)) = self.rtt_done {
                if now >= deadline && self.round > start_round {
                    self.min_rtt.probe_stamp = now;
                    self.reset_short_term();
                    self.cwnd = self.prior_cwnd;
                    if self.filled {
                        self.enter(now, State::Down);
                        self.enter(now, State::Cruise);
                    } else {
                        self.enter(now, State::Startup);
                    }
                }
            } else if in_flight <= self.probe_rtt_cwnd() {
                self.rtt_done = Some((now + Duration::from_millis(200), self.round));
                self.round_end = self.sampler.delivered;
            }
        }
        self.idle_restart = false;
    }

    fn set_controls(&mut self, acked: u64, extra: u64) {
        let rate = pacing_rate(self.bw(), self.state.pacing_gain());
        if self.filled || rate > self.pacing {
            self.pacing = rate
        }
        let gain = if self.state == State::Up { 2.25 } else { 2.0 };
        let quantum = (self.pacing / 1000).clamp(2 * self.mtu, 65536);
        let mut target = ((self.bdp() as f64 * gain) as u64)
            .saturating_add(extra)
            .max(3 * quantum);
        if self.state == State::Up {
            target = target.saturating_add(2 * self.mtu)
        }
        if self.filled {
            self.cwnd = (self.cwnd + acked).min(target)
        } else if self.cwnd < target || self.sampler.delivered < self.initial_window {
            self.cwnd += acked
        }
        let cap = match self.state {
            State::ProbeRtt => self.probe_rtt_cwnd().min(self.headroom()),
            State::Cruise => self.headroom(),
            State::Up | State::Down | State::Refill => self.inflight_hi,
            _ => UNBOUNDED,
        };
        self.cwnd = self.cwnd.min(cap).min(self.inflight_lo).max(4 * self.mtu);
    }

    /// A transmission, before it is added to `in_flight`. Retransmissions
    /// under a reused sequence number must pass `ambiguous`.
    pub(crate) fn on_sent(
        &mut self,
        now: Instant,
        id: u64,
        bytes: u64,
        in_flight: u64,
        app_limited: bool,
        ambiguous: bool,
    ) {
        if app_limited && in_flight == 0 {
            self.idle_restart = true;
            self.aggregation.since = now;
            self.aggregation.bytes = 0;
            if matches!(
                self.state,
                State::Up | State::Down | State::Refill | State::Cruise
            ) {
                self.pacing = pacing_rate(self.bw(), 1.0);
            }
        }
        let probe = matches!(self.state, State::Refill | State::Up).then_some(self.probe_id);
        self.sampler.sent(
            now,
            id,
            bytes,
            in_flight,
            app_limited || self.state == State::ProbeRtt,
            probe,
            ambiguous,
        );
    }

    pub(crate) fn on_acked(&mut self, now: Instant, id: u64) {
        self.sampler.ack(now, id)
    }

    /// Bytes delivered without a live transmission record (already declared lost).
    pub(crate) fn on_delivered_untracked(&mut self, now: Instant, bytes: u64) {
        self.sampler.deliver(now, bytes)
    }

    /// `seq` orders the transport's sequence space (TSN) to count loss ranges.
    pub(crate) fn on_lost(&mut self, now: Instant, id: u64, seq: u64) {
        let Some(p) = self.sampler.lose(id) else {
            return;
        };
        if self.loss.ranges == 0 {
            self.loss.end_delivered = self.sampler.delivered
        }
        if self.loss.last_seq.and_then(|n| n.checked_add(1)) != Some(seq) {
            self.loss.ranges += 1
        }
        self.loss.last_seq = Some(seq);
        self.loss.excessive |= excessive_loss(self.sampler.lost - p.lost_at_send, p.tx_in_flight);
        self.check_probe_loss(now, p, true);
    }

    /// End of one ACK's acked/lost notifications (quinn's on_end_acks).
    pub(crate) fn on_ack_end(&mut self, now: Instant, in_flight: u64) {
        let min_rtt = if self.min_rtt.value == Duration::MAX {
            Duration::ZERO
        } else {
            self.min_rtt.value
        };
        let Some(s) = self.sampler.finish(now, min_rtt) else {
            // Only ambiguous retransmissions or untracked deliveries: no rate,
            // RTT, round or state update, but BBRUpdateControlParameters runs
            // on every ACK, so the window still follows the delivered bytes.
            // (Unreachable from QUIC, where every ACKed packet is a sample.)
            let acked = self.sampler.take_acked();
            if acked > 0 {
                let rounds = if self.filled { 10 } else { 1 };
                let extra =
                    self.aggregation
                        .update(now, acked, self.bw(), self.cwnd, self.round, rounds);
                self.set_controls(acked, extra);
            }
            return;
        };
        let round_start = s.packet.prior_delivered >= self.round_end;
        if round_start {
            self.round += 1;
            self.round_end = self.sampler.delivered;
        }
        if let Some(rate) = s.rate {
            self.max_bw.observe(rate, s.packet.app_limited);
        }
        let expired = self.min_rtt.update(now, s.rtt);
        self.check_probe_loss(now, s.packet, false);
        if self.inflight_hi != UNBOUNDED
            && !excessive_loss(
                self.sampler.lost - s.packet.lost_at_send,
                s.packet.tx_in_flight,
            )
        {
            self.inflight_hi = self.inflight_hi.max(s.packet.tx_in_flight);
        }
        let startup_loss = self.update_loss_round(&s);
        let extra = self.aggregation.update(
            now,
            s.acked,
            self.bw(),
            self.cwnd,
            self.round,
            if self.filled { 10 } else { 1 },
        );
        self.update_state(now, &s, in_flight, round_start, startup_loss);
        self.update_probe_rtt(now, in_flight, expired);
        self.set_controls(s.acked, extra);
    }

    /// BBROnEnterRTO: keep the model, save the cwnd, allow one more segment.
    /// The caller has already declared the outstanding data lost.
    pub(crate) fn on_rto(&mut self, in_flight: u64) {
        self.prior_cwnd = self.save_cwnd();
        self.in_recovery = true;
        self.cwnd = in_flight + self.mtu;
        self.rto_events += 1;
    }

    /// End of RTO recovery (all data outstanding at the RTO repaired): BBRRestoreCwnd.
    pub(crate) fn on_recovery_end(&mut self) {
        if self.in_recovery {
            self.in_recovery = false;
            self.cwnd = self.cwnd.max(self.prior_cwnd);
        }
    }

    pub(crate) fn in_recovery(&self) -> bool {
        self.in_recovery
    }
}

#[cfg(test)]
mod tests;
