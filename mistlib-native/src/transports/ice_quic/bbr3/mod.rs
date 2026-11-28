//! Independent, single-path research implementation of draft-ietf-ccwg-bbr-06.
//! Packet delivery sampling follows draft-cheng-iccrg-delivery-rate-estimation-02.
//! Ported from `tests/quicprobe/src/bbr3` (the measured research controller);
//! see `tests/quicprobe/README.md` for the transport-specific choices and
//! omissions. Requires the patched quinn-proto in `third_party/quinn-proto`.
mod filter;
mod sample;

use std::{
    any::Any,
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use quinn::congestion::{Controller, ControllerFactory, ControllerMetrics};
use quinn_proto::RttEstimator;
use serde::Serialize;
use serde_json::{json, Value};

use filter::{MaxBw, MinRtt};
use sample::{Packet, Sample, Sampler};

// Research identifier and introspection, kept with the ported controller.
#[allow(dead_code)]
pub const NAME: &str = "bbr3-ietf-06";
const INITIAL_RTT: Duration = Duration::from_millis(333); // Quinn's initial estimate
const UNBOUNDED: u64 = u64::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
enum State {
    Startup,
    Drain,
    #[serde(rename = "ProbeBW_DOWN")]
    Down,
    #[serde(rename = "ProbeBW_CRUISE")]
    Cruise,
    #[serde(rename = "ProbeBW_REFILL")]
    Refill,
    #[serde(rename = "ProbeBW_UP")]
    Up,
    #[serde(rename = "ProbeRTT")]
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

#[derive(Clone, Default)]
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

#[derive(Clone, Default)]
struct LossRound {
    end_delivered: u64,
    ranges: u32,
    last_pn: Option<u64>,
    excessive: bool,
    max_bw: u64,
    max_delivered: u64,
}

/// Compressed ACK credit, measured in bytes above the expected delivery rate.
/// Keep the largest excess within each of the last ten packet-timed rounds.
#[derive(Clone)]
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

fn pacing_rate(bw: u64, gain: f64) -> u64 {
    ((bw as f64 * gain * 0.99) as u64).max(1)
}

fn excessive_loss(lost: u64, in_flight: u64) -> bool {
    u128::from(lost) * 50 > u128::from(in_flight)
}

pub struct Factory;

impl ControllerFactory for Factory {
    fn build(self: Arc<Self>, now: Instant, mtu: u16) -> Box<dyn Controller> {
        Box::new(Bbr3::new(now, mtu))
    }
}

#[derive(Clone, Serialize)]
pub struct Transition {
    #[serde(rename = "elapsedS")]
    elapsed_s: f64,
    state: State,
}

#[derive(Clone)]
pub struct Bbr3 {
    state: State,
    start: Instant,
    pub events: Vec<Transition>,
    sampler: Sampler,
    max_bw: MaxBw,
    min_rtt: MinRtt,
    pub quinn_min_rtt: Option<Duration>,
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
    high_loss_probes: u64,
    short_term_cuts: u64,
}

impl Bbr3 {
    fn new(now: Instant, mtu: u16) -> Self {
        let mtu = u64::from(mtu);
        let initial_window = (10 * mtu).min(14720.max(2 * mtu));
        let nominal_bw = (initial_window as f64 / INITIAL_RTT.as_secs_f64()) as u64;
        Self {
            state: State::Startup,
            start: now,
            events: vec![Transition {
                elapsed_s: 0.0,
                state: State::Startup,
            }],
            sampler: Sampler::new(now),
            max_bw: MaxBw::default(),
            min_rtt: MinRtt::new(now),
            quinn_min_rtt: None,
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
            high_loss_probes: 0,
            short_term_cuts: 0,
        }
    }

    #[allow(dead_code)]
    pub fn stats(&self) -> Value {
        let bound = |v| (v != UNBOUNDED).then_some(v);
        json!({
            "state": self.state, "pacingRateBytesPerS": self.pacing, "bwBytesPerS": self.bw(),
            "minRttMs": (self.min_rtt.value != Duration::MAX).then_some(self.min_rtt.value.as_secs_f64() * 1000.0),
            "bwHiBytesPerS": self.max_bw.get(), "bwLoBytesPerS": bound(self.bw_lo),
            "inflightHiBytes": bound(self.inflight_hi), "inflightLoBytes": bound(self.inflight_lo),
            "highLossProbes": self.high_loss_probes, "shortTermLossResponses": self.short_term_cuts,
        })
    }

    fn bw(&self) -> u64 {
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

    fn enter(&mut self, now: Instant, state: State) {
        self.state = state;
        self.state_round = self.round;
        self.events.push(Transition {
            elapsed_s: (now - self.start).as_secs_f64(),
            state,
        });
        match state {
            State::Down => {
                self.cycle_start_round = self.round;
                self.probe_after = now
                    + Duration::from_millis(rand::Rng::gen_range(
                        &mut rand::thread_rng(),
                        2000..3000,
                    ));
                self.probe_wait_rounds = rand::Rng::gen_range(&mut rand::thread_rng(), 62..=63);
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
            State::ProbeRtt => {
                self.prior_cwnd = self.cwnd;
                self.rtt_done = None;
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
                let size = u64::from(p.bytes);
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
}

impl Controller for Bbr3 {
    fn pacing_rate(&self) -> Option<u64> {
        Some(self.pacing)
    }

    fn on_data_sent(
        &mut self,
        now: Instant,
        pn: u64,
        bytes: u16,
        in_flight: u64,
        app_limited: bool,
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
            pn,
            bytes,
            in_flight,
            app_limited || self.state == State::ProbeRtt,
            probe,
        );
    }

    fn on_data_acked(&mut self, now: Instant, pn: u64) {
        self.sampler.ack(now, pn)
    }

    fn on_data_lost(&mut self, now: Instant, pn: u64) {
        let Some(p) = self.sampler.lose(pn) else {
            return;
        };
        if self.loss.ranges == 0 {
            self.loss.end_delivered = self.sampler.delivered
        }
        if self.loss.last_pn.and_then(|n| n.checked_add(1)) != Some(pn) {
            self.loss.ranges += 1
        }
        self.loss.last_pn = Some(pn);
        self.loss.excessive |= excessive_loss(self.sampler.lost - p.lost_at_send, p.tx_in_flight);
        self.check_probe_loss(now, p, true);
    }

    fn on_ack(&mut self, _: Instant, _: Instant, _: u64, _: bool, rtt: &RttEstimator) {
        self.quinn_min_rtt = Some(rtt.min());
    }
    fn on_end_acks(&mut self, now: Instant, in_flight: u64, _: bool, _: Option<u64>) {
        let min_rtt = if self.min_rtt.value == Duration::MAX {
            Duration::ZERO
        } else {
            self.min_rtt.value
        };
        let Some(s) = self.sampler.finish(now, min_rtt) else {
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

    fn on_congestion_event(&mut self, _: Instant, _: Instant, persistent: bool, _: u64) {
        if persistent {
            self.cwnd = 4 * self.mtu
        }
    }
    fn on_mtu_update(&mut self, mtu: u16) {
        self.mtu = u64::from(mtu)
    }
    fn window(&self) -> u64 {
        self.cwnd
    }
    fn initial_window(&self) -> u64 {
        self.initial_window
    }
    fn metrics(&self) -> ControllerMetrics {
        let mut metrics = ControllerMetrics::default();
        metrics.congestion_window = self.cwnd;
        metrics.pacing_rate = Some(self.pacing.saturating_mul(8));
        metrics
    }
    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests;
