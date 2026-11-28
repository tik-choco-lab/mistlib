//! Model tests ported from tests/quicprobe/src/bbr3/tests.rs, plus the RTO response.
use super::*;

// One transmission, its ACK and the end of that ACK.
fn ack_round(bbr: &mut Bbr3, now: Instant, id: u64, bytes: u64, backlog: u64) {
    let rtt = Duration::from_millis(100);
    bbr.on_sent(now, id, bytes, 0, false, false);
    bbr.on_acked(now + rtt, id);
    bbr.on_ack_end(now + rtt, backlog);
}

#[test]
fn plateau_drain_and_probe_cycle() {
    let now = Instant::now();
    let ms = Duration::from_millis;
    let mut b = Bbr3::new(now, 1200);
    // Equal delivery rates for one baseline round and three plateau rounds.
    for id in 0..4 {
        ack_round(&mut b, now + ms(id * 100), id, 1200, 40_000);
        if id < 3 {
            assert_eq!(b.state, State::Startup)
        }
    }
    assert_eq!(b.state, State::Drain);
    ack_round(&mut b, now + ms(400), 4, 1200, 0);
    assert_eq!(b.state, State::Down);
    // Control the Reno bound by seeding a larger capacity estimate.
    b.max_bw.observe(1_200_000, false);
    b.cwnd = 100_000;
    ack_round(&mut b, now + ms(500), 5, 1200, 0);
    assert_eq!(b.state, State::Cruise);
    let after = b.probe_after;
    ack_round(&mut b, after, 6, 1200, 0);
    assert_eq!(b.state, State::Refill);
    ack_round(&mut b, after + ms(100), 7, 1200, 0);
    assert_eq!(b.state, State::Up);
    for id in 8..12 {
        ack_round(&mut b, after + ms((id - 6) * 100), id, 1200, 0)
    }
    assert_eq!(b.state, State::Down);
}

#[test]
fn probe_rtt_requires_low_flight_time_and_round() {
    let now = Instant::now();
    let ms = Duration::from_millis;
    let mut b = Bbr3::new(now, 1200);
    b.filled = true;
    b.enter(now, State::Cruise);
    ack_round(&mut b, now, 1, 1200, 10_000);
    ack_round(&mut b, now + ms(5100), 2, 1200, 10_000);
    assert_eq!(b.state, State::ProbeRtt);
    assert!(b.rtt_done.is_none());
    ack_round(&mut b, now + ms(5300), 3, 1200, 0);
    let (deadline, _) = b.rtt_done.unwrap();
    b.update_probe_rtt(deadline, 0, false);
    assert_eq!(b.state, State::ProbeRtt);
    b.round += 1;
    b.update_probe_rtt(deadline - ms(1), 0, false);
    assert_eq!(b.state, State::ProbeRtt);
    b.update_probe_rtt(deadline, 0, false);
    assert_eq!(b.state, State::Cruise);
}

#[test]
fn two_percent_is_strict_and_late_losses_only_cut_once() {
    let now = Instant::now();
    let mut b = Bbr3::new(now, 1200);
    assert!(!excessive_loss(1000, 50_000));
    assert!(excessive_loss(1001, 50_000));
    b.enter(now, State::Refill);
    b.enter(now, State::Up);
    for id in 0..3 {
        b.on_sent(now, id, 1000, 49_000, false, false)
    }
    b.on_lost(now, 0, 0);
    assert_eq!(b.inflight_hi, UNBOUNDED);
    b.on_lost(now, 1, 1);
    assert_eq!(b.state, State::Down);
    assert_eq!(b.inflight_hi, 49_000);
    b.on_lost(now, 2, 2);
    assert_eq!(b.high_loss_probes, 1);
}

#[test]
fn pacing_uses_model_gain_and_one_percent_margin() {
    assert_eq!(pacing_rate(1_000_000, State::Cruise.pacing_gain()), 990_000);
    assert_eq!(pacing_rate(1_000_000, State::Up.pacing_gain()), 1_237_500);
    assert_eq!(pacing_rate(1_000_000, State::Drain.pacing_gain()), 495_000);
}

#[test]
fn startup_high_loss_waits_for_six_ranges_and_a_delivery_round() {
    let now = Instant::now();
    let later = now + Duration::from_millis(100);
    for ranges in [5, 6] {
        let mut b = Bbr3::new(now, 1200);
        // Establish the pre-loss delivered boundary. One older packet is
        // still in flight when the losses arrive.
        b.on_sent(now, 0, 1200, 0, false, false);
        b.on_sent(now, 1, 1200, 1200, false, false);
        for i in 0..ranges {
            b.on_sent(now, 4 + 2 * i, 1200, 50_000, false, false);
        }
        b.on_acked(later, 0);
        b.on_ack_end(later, 50_000);
        for i in 0..ranges {
            b.on_lost(later, 4 + 2 * i, 4 + 2 * i);
        }
        b.on_acked(later, 1);
        b.on_ack_end(later, 50_000);
        assert_eq!(b.state, State::Startup);
        ack_round(&mut b, later, 100, 1200, 50_000);
        assert_eq!(
            b.state,
            if ranges == 6 {
                State::Drain
            } else {
                State::Startup
            }
        );
        assert_eq!(b.inflight_hi != UNBOUNDED, ranges == 6);
    }
}

#[test]
fn app_limited_plateau_does_not_exit_startup() {
    let now = Instant::now();
    let mut b = Bbr3::new(now, 1200);
    for id in 0..10 {
        let sent = now + Duration::from_millis(100 * id);
        let acked = sent + Duration::from_millis(100);
        b.on_sent(sent, id, 1200, 0, true, false);
        b.on_acked(acked, id);
        b.on_ack_end(acked, 0);
    }
    assert_eq!(b.state, State::Startup);
    assert_eq!(b.max_bw.get(), 12_000);
}

#[test]
fn rto_keeps_the_model_and_restores_the_saved_cwnd() {
    let now = Instant::now();
    let ms = Duration::from_millis;
    let mut b = Bbr3::new(now, 1200);
    b.filled = true;
    b.enter(now, State::Cruise);
    b.cwnd = 60_000;
    b.max_bw.observe(1_000_000, false);
    b.on_rto(0);
    assert!(b.in_recovery());
    assert_eq!(b.cwnd(), 1200);
    assert_eq!(b.max_bw.get(), 1_000_000);
    // ACKs during recovery grow from the minimum pipe, bounded by the model.
    ack_round(&mut b, now + ms(10), 1, 1200, 0);
    assert_eq!(b.cwnd(), 4 * 1200);
    // A second RTO keeps the larger saved value.
    b.on_rto(0);
    b.on_recovery_end();
    assert!(!b.in_recovery());
    assert_eq!(b.cwnd(), 60_000);
    assert_eq!(b.rto_events, 2);
    // Restoration is idempotent outside recovery.
    b.cwnd = 10_000;
    b.on_recovery_end();
    assert_eq!(b.cwnd(), 10_000);
}
