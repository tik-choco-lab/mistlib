//! In-process loopback tests: two links connected through an in-memory
//! signaling exchange (JSON round trip), no external network.

use std::collections::BTreeSet;
use std::time::Duration;

use bytes::Bytes;
use mistlib_core::types::DeliveryMethod;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::{mpsc, watch};

use super::link::Incoming;
use super::*;

/// Generous: CI machines can be slow, and nothing here waits on a timer.
const STEP: Duration = Duration::from_secs(30);

fn config() -> IceQuicConfig {
    IceQuicConfig {
        loopback_only: true,
        connect_timeout: Duration::from_secs(20),
        ..IceQuicConfig::default()
    }
}

/// The in-memory signaling channel: what the peer receives is what JSON
/// carries.
fn signal<T: Serialize + DeserializeOwned>(payload: &T) -> T {
    let json = serde_json::to_string(payload).unwrap();
    serde_json::from_str(&json).unwrap()
}

async fn step<F: std::future::Future>(what: &str, future: F) -> F::Output {
    tokio::time::timeout(STEP, future)
        .await
        .unwrap_or_else(|_| panic!("{what} timed out"))
}

/// (offerer link, answerer link).
async fn pair_with(offerer: IceQuicConfig, answerer: IceQuicConfig) -> (IceQuicLink, IceQuicLink) {
    let offerer = step("offerer gather", IceQuicOfferer::gather(offerer))
        .await
        .unwrap();
    let answerer = step(
        "answerer gather",
        IceQuicAnswerer::gather(answerer, signal(&offerer.offer())),
    )
    .await
    .unwrap();
    let answer = signal(&answerer.answer());
    let (a, b) = step("connect", async {
        tokio::join!(offerer.connect(answer), answerer.connect())
    })
    .await;
    (a.unwrap(), b.unwrap())
}

async fn pair() -> (IceQuicLink, IceQuicLink) {
    pair_with(config(), config()).await
}

async fn recv(rx: &mut mpsc::Receiver<Incoming>) -> Incoming {
    step("receive", rx.recv())
        .await
        .expect("link ended while a message was expected")
}

async fn wait_state(state: &mut watch::Receiver<IceQuicState>, want: IceQuicState) {
    step("state change", state.wait_for(|s| *s == want))
        .await
        .unwrap_or_else(|_| panic!("state channel closed before {want:?}"));
}

fn indexed(i: u32, len: usize) -> Bytes {
    let mut data = vec![(i % 251) as u8; len.max(4)];
    data[..4].copy_from_slice(&i.to_be_bytes());
    Bytes::from(data)
}

fn index(data: &[u8]) -> u32 {
    u32::from_be_bytes(data[..4].try_into().unwrap())
}

#[test]
fn signaling_payload_is_camel_case_and_validated() {
    let params = IceQuicParams {
        version: PROTOCOL_VERSION.into(),
        ice_ufrag: "ufrag".into(),
        ice_pwd: "password-password".into(),
        candidates: vec!["1 1 udp 2130706431 127.0.0.1 5000 typ host".into()],
        cert_sha256: "ab".repeat(32),
        max_message_bytes: 65_536,
    };
    let json = serde_json::to_value(IceQuicOffer(params.clone())).unwrap();
    let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "candidates",
            "certSha256",
            "icePwd",
            "iceUfrag",
            "maxMessageBytes",
            "version",
        ]
    );
    assert_eq!(json["version"], "iq1");
    assert_eq!(signal(&IceQuicAnswer(params.clone())).0, params);
    params.validate().unwrap();

    let invalid = [
        IceQuicParams {
            version: "iq2".into(),
            ..params.clone()
        },
        IceQuicParams {
            cert_sha256: "AB".repeat(32),
            ..params.clone()
        },
        IceQuicParams {
            candidates: Vec::new(),
            ..params.clone()
        },
        IceQuicParams {
            ice_pwd: String::new(),
            ..params.clone()
        },
        IceQuicParams {
            max_message_bytes: 0,
            ..params.clone()
        },
    ];
    for bad in invalid {
        assert!(
            matches!(bad.validate(), Err(IceQuicError::Signal(_))),
            "{bad:?} must be rejected"
        );
    }
}

#[test]
fn ice_servers_follow_the_webrtc_config_shape() {
    let config = IceQuicConfig::with_ice_servers(&[
        mistlib_core::config::IceServer {
            urls: vec!["stun:stun.example.invalid:3478".into()],
            username: None,
            credential: None,
        },
        // Dropped by map_ice_servers, as for WebRTC: TURN without credentials.
        mistlib_core::config::IceServer {
            urls: vec!["turn:turn.example.invalid:3478".into()],
            username: None,
            credential: None,
        },
        mistlib_core::config::IceServer {
            urls: vec!["turn:turn.example.invalid:3478?transport=udp".into()],
            username: Some("user".into()),
            credential: Some("secret".into()),
        },
    ]);
    let urls = config.ice_urls();
    assert_eq!(urls.len(), 2);
    assert_eq!(urls[0].host, "stun.example.invalid");
    assert_eq!(urls[1].username, "user");
    assert_eq!(urls[1].password, "secret");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reliable_ordered_preserves_order() {
    let (a, b) = pair().await;
    let mut a_state = a.state();
    assert_eq!(*a_state.borrow_and_update(), IceQuicState::Connected);
    let mut rx = b.take_receiver().unwrap();
    assert!(b.take_receiver().is_none());

    const COUNT: u32 = 2_000;
    let sender = async {
        for i in 0..COUNT {
            // Sizes cross packet boundaries so frames straddle packets.
            let len = 4 + (i as usize * 37) % 3_000;
            a.send(DeliveryMethod::ReliableOrdered, indexed(i, len))
                .await
                .unwrap();
        }
    };
    let receiver = async {
        for i in 0..COUNT {
            let (method, data) = recv(&mut rx).await;
            assert_eq!(method, DeliveryMethod::ReliableOrdered);
            assert_eq!(index(&data), i, "ordered stream reordered");
            assert_eq!(data.len(), 4 + (i as usize * 37) % 3_000);
        }
    };
    tokio::join!(sender, receiver);

    // And the other direction on the same stream.
    let mut back = a.take_receiver().unwrap();
    b.send(DeliveryMethod::ReliableOrdered, Bytes::from_static(b"pong"))
        .await
        .unwrap();
    assert_eq!(
        recv(&mut back).await,
        (DeliveryMethod::ReliableOrdered, Bytes::from_static(b"pong"))
    );

    let stats: IceQuicStats = a.stats();
    assert!(stats.sent_packets > 0 && stats.bytes_sent > 0);
    assert!(stats.bytes_received > 0);
    assert!(stats.cwnd > 0);
    assert!(stats.rtt > Duration::ZERO);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reliable_unordered_delivers_every_message() {
    let (a, b) = pair().await;
    let mut rx = b.take_receiver().unwrap();
    const COUNT: u32 = 500;
    let sender = async {
        for i in 0..COUNT {
            let data = indexed(i, 64 + i as usize);
            a.send(DeliveryMethod::ReliableUnordered, data)
                .await
                .unwrap();
        }
    };
    let receiver = async {
        let mut seen = BTreeSet::new();
        for _ in 0..COUNT {
            let (method, data) = recv(&mut rx).await;
            assert_eq!(method, DeliveryMethod::ReliableUnordered);
            let i = index(&data);
            assert_eq!(data.len(), 64 + i as usize);
            assert!(seen.insert(i), "message {i} delivered twice");
        }
        seen
    };
    let ((), seen) = tokio::join!(sender, receiver);
    assert_eq!(seen, (0..COUNT).collect());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn datagrams_deliver_on_loopback() {
    let (a, b) = pair().await;
    let mut rx = b.take_receiver().unwrap();

    let limit = a
        .message_size_limit(DeliveryMethod::Unreliable)
        .expect("DATAGRAM negotiated");
    assert!((1_000..1_200).contains(&limit), "limit {limit}");
    assert_eq!(
        a.max_datagram_payload(DeliveryMethod::UnreliableOrdered),
        Some(limit - 2)
    );
    assert_eq!(
        a.message_size_limit(DeliveryMethod::ReliableOrdered),
        Some(DEFAULT_MAX_MESSAGE_BYTES as usize)
    );
    // The largest payload fits; one byte more is an error, not a fragment.
    let largest = Bytes::from(vec![7; limit]);
    a.send(DeliveryMethod::Unreliable, largest.clone())
        .await
        .unwrap();
    let oversized = Bytes::from(vec![7; limit + 1]);
    let result = a.send(DeliveryMethod::Unreliable, oversized).await;
    let Err(IceQuicError::TooLarge { size, limit: l, .. }) = result else {
        panic!("{result:?}");
    };
    assert_eq!((size, l), (limit + 1, limit));
    assert_eq!(recv(&mut rx).await, (DeliveryMethod::Unreliable, largest));

    const COUNT: u32 = 100;
    for i in 0..COUNT {
        a.send(DeliveryMethod::Unreliable, indexed(i, 64))
            .await
            .unwrap();
    }
    let mut seen = BTreeSet::new();
    while seen.len() < COUNT as usize {
        let (method, data) = recv(&mut rx).await;
        assert_eq!(method, DeliveryMethod::Unreliable);
        seen.insert(index(&data));
    }

    for i in 0..COUNT {
        a.send(DeliveryMethod::UnreliableOrdered, indexed(i, 32))
            .await
            .unwrap();
    }
    // Late datagrams may be dropped, never delivered out of order; on
    // loopback the last one arrives.
    let mut last = None;
    while last != Some(COUNT - 1) {
        let (method, data) = recv(&mut rx).await;
        assert_eq!(method, DeliveryMethod::UnreliableOrdered);
        let i = index(&data);
        assert!(last.is_none_or(|l| i > l), "{i} after {last:?}");
        last = Some(i);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fingerprint_mismatch_is_rejected() {
    let offerer = step("gather", IceQuicOfferer::gather(config()))
        .await
        .unwrap();
    let answerer = step(
        "gather",
        IceQuicAnswerer::gather(config(), signal(&offerer.offer())),
    )
    .await
    .unwrap();
    let mut offerer_state = offerer.state();
    let mut answerer_state = answerer.state();
    assert_eq!(*answerer_state.borrow(), IceQuicState::Connecting);
    let mut answer = signal(&answerer.answer());
    // A valid-looking fingerprint of some other certificate.
    answer.0.cert_sha256 = if answer.0.cert_sha256.starts_with('0') {
        "1".repeat(64)
    } else {
        "0".repeat(64)
    };
    let (a, b) = step("connect", async {
        tokio::join!(offerer.connect(answer), answerer.connect())
    })
    .await;
    assert!(
        matches!(a, Err(IceQuicError::FingerprintMismatch)),
        "offerer: {:?}",
        a.err()
    );
    assert!(b.is_err(), "answerer must not connect to a rejected peer");
    wait_state(&mut offerer_state, IceQuicState::Failed).await;
    wait_state(&mut answerer_state, IceQuicState::Failed).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_is_observed_by_peer() {
    let (a, b) = pair().await;
    let mut rx = b.take_receiver().unwrap();
    let mut b_state = b.state();
    a.send(DeliveryMethod::ReliableOrdered, Bytes::from_static(b"bye"))
        .await
        .unwrap();
    assert_eq!(recv(&mut rx).await.1, Bytes::from_static(b"bye"));

    step("close", a.close()).await;
    assert_eq!(*a.state().borrow(), IceQuicState::Disconnected);
    // A normal close is Disconnected, not Failed, and arrives well before
    // the idle timeout would.
    wait_state(&mut b_state, IceQuicState::Disconnected).await;
    assert!(matches!(
        b.close_reason(),
        Some(quinn::ConnectionError::ApplicationClosed(_))
    ));
    assert!(step("receiver end", rx.recv()).await.is_none());
    assert!(b
        .send(DeliveryMethod::ReliableOrdered, Bytes::from_static(b"x"))
        .await
        .is_err());
    assert!(a
        .send(DeliveryMethod::Unreliable, Bytes::from_static(b"x"))
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_a_link_disconnects_the_peer() {
    let (a, b) = pair().await;
    let mut b_state = b.state();
    drop(a);
    wait_state(&mut b_state, IceQuicState::Disconnected).await;
}

/// ICE keepalives are off and ICE's disconnected/failed timeouts equal the
/// QUIC idle timeout, so ICE relies on QUIC PINGs refreshing the pair's
/// last-received time. An idle link must outlive ICE's whole failure window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_alive_holds_an_idle_link_past_the_ice_failure_window() {
    let short = IceQuicConfig {
        keep_alive: Some(Duration::from_millis(300)),
        idle_timeout: Duration::from_millis(1_500),
        ..config()
    };
    let (a, b) = pair_with(short.clone(), short).await;
    let mut rx = b.take_receiver().unwrap();
    // ICE would fail after 1.5 s disconnected + 1.5 s failed without traffic.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(*a.state().borrow(), IceQuicState::Connected);
    assert_eq!(*b.state().borrow(), IceQuicState::Connected);
    let message = Bytes::from_static(b"still here");
    a.send(DeliveryMethod::ReliableOrdered, message.clone())
        .await
        .unwrap();
    assert_eq!(recv(&mut rx).await.1, message);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silence_without_keep_alive_fails_the_link() {
    let silent = IceQuicConfig {
        keep_alive: None,
        idle_timeout: Duration::from_millis(1_000),
        ..config()
    };
    let (a, b) = pair_with(silent.clone(), silent).await;
    let (mut a_state, mut b_state) = (a.state(), b.state());
    wait_state(&mut a_state, IceQuicState::Failed).await;
    wait_state(&mut b_state, IceQuicState::Failed).await;
    assert!(a
        .send(DeliveryMethod::ReliableOrdered, Bytes::from_static(b"x"))
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_reliable_messages() {
    const LARGE: usize = 4 * 1024 * 1024;
    let big = IceQuicConfig {
        max_message_bytes: 8 * 1024 * 1024,
        ..config()
    };
    let (a, b) = pair_with(big.clone(), big).await;
    let mut rx = b.take_receiver().unwrap();
    let payload = Bytes::from((0..LARGE).map(|i| (i % 253) as u8).collect::<Vec<_>>());
    for method in [
        DeliveryMethod::ReliableOrdered,
        DeliveryMethod::ReliableUnordered,
    ] {
        let (sent, received) = tokio::join!(a.send(method, payload.clone()), recv(&mut rx));
        sent.unwrap();
        assert_eq!(received.0, method);
        assert!(received.1 == payload, "{method:?} payload corrupted");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reliable_limit_is_the_smaller_of_both_sides() {
    let small = IceQuicConfig {
        max_message_bytes: 1_024,
        ..config()
    };
    // Offerer allows 64 KiB, answerer 1 KiB: both send at most 1 KiB.
    let (a, b) = pair_with(config(), small).await;
    let mut rx = b.take_receiver().unwrap();
    for link in [&a, &b] {
        assert_eq!(
            link.message_size_limit(DeliveryMethod::ReliableUnordered),
            Some(1_024)
        );
    }
    let oversized = Bytes::from(vec![0; 1_025]);
    let result = a.send(DeliveryMethod::ReliableOrdered, oversized).await;
    assert!(
        matches!(result, Err(IceQuicError::TooLarge { limit: 1_024, .. })),
        "{result:?}"
    );
    a.send(DeliveryMethod::ReliableOrdered, Bytes::from(vec![1; 1_024]))
        .await
        .unwrap();
    assert_eq!(recv(&mut rx).await.1.len(), 1_024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bbr_congestion_control_connects_and_delivers() {
    let bbr = IceQuicConfig {
        congestion: IceQuicCongestion::Bbr,
        ..config()
    };
    let (a, b) = pair_with(bbr.clone(), bbr).await;
    let mut rx = b.take_receiver().unwrap();
    for i in 0..200 {
        a.send(DeliveryMethod::ReliableOrdered, indexed(i, 8_000))
            .await
            .unwrap();
    }
    for i in 0..200 {
        assert_eq!(index(&recv(&mut rx).await.1), i);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_path_that_drops_quic_fails_fast_on_both_sides() {
    // ICE (STUN) works, but nothing the offerer sends over QUIC arrives: both
    // sides hear no QUIC datagram and give up after the silence timeout,
    // long before the connect timeout.
    let blocked = IceQuicConfig {
        drop_outgoing_quic: true,
        ..config()
    };
    let offerer = step("offerer gather", IceQuicOfferer::gather(blocked))
        .await
        .unwrap();
    let answerer = step(
        "answerer gather",
        IceQuicAnswerer::gather(config(), signal(&offerer.offer())),
    )
    .await
    .unwrap();
    let answer = signal(&answerer.answer());
    let started = std::time::Instant::now();
    let (a, b) = step("connect", async {
        tokio::join!(offerer.connect(answer), answerer.connect())
    })
    .await;
    let elapsed = started.elapsed();
    for (side, result) in [("offerer", a), ("answerer", b)] {
        match result {
            Err(IceQuicError::QuicSilent(timeout)) => {
                assert_eq!(timeout, DEFAULT_QUIC_SILENCE_TIMEOUT)
            }
            Err(other) => panic!("{side}: expected QuicSilent, got {other}"),
            Ok(_) => panic!("{side}: connected over a path that drops QUIC"),
        }
    }
    assert!(elapsed < config().connect_timeout, "took {elapsed:?}");
}
