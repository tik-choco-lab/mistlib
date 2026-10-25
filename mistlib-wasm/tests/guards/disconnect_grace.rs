#[path = "../../src/transport/webrtc/disconnect_grace.rs"]
mod implementation;

use implementation::{decide_grace_expiry, GraceExpiryDecision, InboundActivity};
use mistlib_core::types::ConnectionState;
use std::time::Duration;
use web_sys::RtcIceConnectionState;
use web_time::Instant;

#[test]
fn inbound_silence_grace_decision_table() {
    use GraceExpiryDecision::{ExtendedByTraffic, Reap, Wait};
    use RtcIceConnectionState::{Closed, Connected, Disconnected, Failed};
    let now = Instant::now();
    let start = now - Duration::from_millis(1_000);
    let recent = now - Duration::from_millis(10);
    let boundary = now - Duration::from_millis(50);
    for (name, ice, state, grace_start, rx, expected) in [
        (
            "silent grace boundary",
            Disconnected,
            ConnectionState::Reconnecting,
            Some(boundary),
            None,
            Reap,
        ),
        (
            "long disconnect with recent traffic",
            Disconnected,
            ConnectionState::Reconnecting,
            Some(start),
            Some(recent),
            ExtendedByTraffic,
        ),
        (
            "logical reconnect with recent traffic",
            Connected,
            ConnectionState::Reconnecting,
            Some(start),
            Some(recent),
            ExtendedByTraffic,
        ),
        (
            "traffic before grace does not shorten it",
            Disconnected,
            ConnectionState::Reconnecting,
            Some(recent),
            Some(start),
            Wait,
        ),
        (
            "last traffic exactly one window ago",
            Disconnected,
            ConnectionState::Reconnecting,
            Some(start),
            Some(boundary),
            Reap,
        ),
        (
            "connected stale grace",
            Connected,
            ConnectionState::Connected,
            Some(start),
            None,
            Wait,
        ),
        (
            "connected recent traffic",
            Connected,
            ConnectionState::Connected,
            Some(start),
            Some(recent),
            Wait,
        ),
        (
            "failed with traffic",
            Failed,
            ConnectionState::Reconnecting,
            Some(recent),
            Some(now),
            Reap,
        ),
        (
            "closed with traffic",
            Closed,
            ConnectionState::Reconnecting,
            Some(recent),
            Some(now),
            Reap,
        ),
        (
            "failed without grace",
            Failed,
            ConnectionState::Connected,
            None,
            Some(now),
            Reap,
        ),
        (
            "closed without grace",
            Closed,
            ConnectionState::Connected,
            None,
            Some(now),
            Reap,
        ),
        (
            "disconnected without grace",
            Disconnected,
            ConnectionState::Reconnecting,
            None,
            Some(now),
            Wait,
        ),
    ] {
        assert_eq!(
            decide_grace_expiry(ice, state, grace_start, rx, now, 50),
            expected,
            "{name}"
        );
    }
}

#[test]
fn activity_and_extension_log_are_scoped_to_peer_and_grace() {
    let activity = InboundActivity::new();
    let other_peer = InboundActivity::new();
    assert_eq!(activity.last_rx(), None);
    activity.record();
    let first_rx = activity.last_rx().unwrap();
    activity.record();
    assert!(activity.last_rx().unwrap() >= first_rx);
    assert_eq!(other_peer.last_rx(), None);

    let start = Instant::now();
    assert!(activity.log_extension_once(start));
    assert!(!activity.log_extension_once(start));
    assert!(activity.log_extension_once(start + Duration::from_secs(1)));
}

#[test]
fn transport_byte_growth_extends_grace_but_baselines_do_not() {
    let activity = InboundActivity::new();
    assert!(!activity.record_transport_bytes("pair", 100));
    assert_eq!(activity.last_rx(), None);
    assert!(!activity.record_transport_bytes("pair", 100));
    assert!(!activity.record_transport_bytes("pair", 1));
    assert_eq!(activity.last_rx(), None);
    assert!(activity.record_transport_bytes("pair", 2));
    let rx = activity.last_rx().unwrap();
    assert_eq!(
        decide_grace_expiry(
            RtcIceConnectionState::Disconnected,
            ConnectionState::Reconnecting,
            Some(rx - Duration::from_secs(1)),
            Some(rx),
            rx,
            50,
        ),
        GraceExpiryDecision::ExtendedByTraffic
    );
    assert_eq!(
        decide_grace_expiry(
            RtcIceConnectionState::Disconnected,
            ConnectionState::Reconnecting,
            Some(rx - Duration::from_secs(1)),
            Some(rx),
            rx + Duration::from_millis(50),
            50,
        ),
        GraceExpiryDecision::Reap
    );
    assert!(!activity.record_transport_bytes("new-pair", 1000));
    assert_eq!(activity.last_rx(), Some(rx));
    assert!(!InboundActivity::new().record_transport_bytes("pair", 1000));
}
