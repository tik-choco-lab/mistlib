use super::*;

// Drive the same packet and ACK-batch hooks that Quinn uses.
fn ack_round(bbr: &mut Bbr3, now: Instant, pn: u64, bytes: u16, backlog: u64) {
    let rtt = Duration::from_millis(100);
    bbr.on_data_sent(now, pn, bytes, 0, false);
    bbr.on_data_acked(now + rtt, pn);
    bbr.on_end_acks(now + rtt, backlog, false, Some(pn));
}

#[test]
fn plateau_drain_and_probe_cycle() {
    let now = Instant::now();
    let ms = Duration::from_millis;
    let mut b = Bbr3::new(now, 1200);
    // Equal delivery rates for one baseline round and three plateau rounds.
    for pn in 0..4 {
        ack_round(&mut b, now + ms(pn * 100), pn, 1200, 40_000);
        if pn < 3 {
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
    for pn in 8..12 {
        ack_round(&mut b, after + ms((pn - 6) * 100), pn, 1200, 0)
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
    for pn in 0..3 {
        b.on_data_sent(now, pn, 1000, 49_000, false)
    }
    b.on_data_lost(now, 0);
    assert_eq!(b.inflight_hi, UNBOUNDED);
    b.on_data_lost(now, 1);
    assert_eq!(b.state, State::Down);
    assert_eq!(b.inflight_hi, 49_000);
    b.on_data_lost(now, 2);
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
        b.on_data_sent(now, 0, 1200, 0, false);
        b.on_data_sent(now, 1, 1200, 1200, false);
        for i in 0..ranges {
            b.on_data_sent(now, 4 + 2 * i, 1200, 50_000, false);
        }
        b.on_data_acked(later, 0);
        b.on_end_acks(later, 50_000, false, Some(0));
        for i in 0..ranges {
            b.on_data_lost(later, 4 + 2 * i);
        }
        b.on_data_acked(later, 1);
        b.on_end_acks(later, 50_000, false, Some(1));
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
    for pn in 0..10 {
        let sent = now + Duration::from_millis(100 * pn);
        let acked = sent + Duration::from_millis(100);
        b.on_data_sent(sent, pn, 1200, 0, true);
        b.on_data_acked(acked, pn);
        b.on_end_acks(acked, 0, true, Some(pn));
    }
    assert_eq!(b.state, State::Startup);
    assert_eq!(b.max_bw.get(), 12_000);
}
