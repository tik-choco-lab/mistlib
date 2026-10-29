use super::{config, extract_sub_id, NostrSignaler};
use crate::signaling::nostr::connection::RELAY_CONNECT_TIMEOUT;
use futures_util::{SinkExt, StreamExt};
use mistlib_core::signaling::Signaler;
use mistlib_core::types::NodeId;
use std::collections::HashMap;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{timeout, timeout_at, Duration, Instant};
use tokio_tungstenite::{accept_async, tungstenite::Message};

async fn rejecting_relay(
    reason: &'static str,
    copies: usize,
) -> (String, mpsc::Receiver<String>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel(128);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        while let Some(Ok(message)) = ws.next().await {
            if let Message::Text(text) = message {
                if text.starts_with("[\"REQ\"") {
                    let id = extract_sub_id(&text);
                    if tx.send(id.clone()).await.is_err() {
                        break;
                    }
                    let closed = serde_json::json!(["CLOSED", id, reason]).to_string();
                    for _ in 0..copies {
                        if ws.send(Message::Text(closed.clone().into())).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
    (url, rx, server)
}

async fn connect_room(url: String, periodic: bool) -> NostrSignaler {
    let mut cfg = config();
    cfg.relays = vec![url];
    let signaler = NostrSignaler::new(NodeId("retry-test".into()), cfg);
    signaler.set_room_id("retry-room").await.unwrap();
    if !periodic {
        signaler.cancel_room_resubscribe();
    }
    signaler.connect(mpsc::channel(8).0).await.unwrap();
    signaler
}

async fn collect_requests(rx: &mut mpsc::Receiver<String>, duration: Duration) -> Vec<String> {
    let deadline = Instant::now() + duration;
    let mut requests = Vec::new();
    while let Ok(Some(id)) = timeout_at(deadline, rx.recv()).await {
        requests.push(id);
        assert!(requests.len() < 128, "CLOSED amplified REQ traffic");
    }
    requests
}

#[tokio::test]
async fn repeated_rate_limited_closed_is_bounded_and_coalesced_per_subscription() {
    let (url, mut requests, server) = rejecting_relay("rate-limited: slow down", 16).await;
    let signaler = connect_room(url, false).await;
    let frames = collect_requests(&mut requests, Duration::from_secs(3)).await;
    signaler.close().await.unwrap();
    server.abort();

    let mut counts = HashMap::new();
    for id in frames {
        *counts.entry(id).or_insert(0) += 1;
    }
    assert_eq!(counts.len(), 2);
    // Initial two REQs, then at most two retries each (>=0.8s, >=1.6s).
    // Duplicate CLOSED frames must neither reserve more work nor burn budget.
    assert!(counts.values().all(|count| (2..=3).contains(count)));
}

#[tokio::test]
async fn auth_required_closed_does_not_retry() {
    let (url, mut requests, server) = rejecting_relay("auth-required: login", 16).await;
    let signaler = connect_room(url, false).await;
    let frames = collect_requests(&mut requests, Duration::from_millis(1500)).await;
    signaler.close().await.unwrap();
    server.abort();
    assert_eq!(frames.len(), 2);
    assert_ne!(frames[0], frames[1]);
}

#[tokio::test]
async fn permanent_closed_does_not_amplify_periodic_resubscribe() {
    let (url, mut requests, server) = rejecting_relay("auth-required: login", 16).await;
    let signaler = connect_room(url, true).await;
    let frames = collect_requests(&mut requests, Duration::from_millis(1550)).await;
    signaler.close().await.unwrap();
    server.abort();
    // Test builds refresh both filters every 300ms. CLOSED adds no traffic.
    assert!((10..=12).contains(&frames.len()), "{} REQs", frames.len());
}

#[tokio::test]
async fn eose_and_event_reset_closed_retry_backoff() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let mut cfg = config();
    cfg.relays = vec![format!("ws://{}", listener.local_addr().unwrap())];
    let signaler = NostrSignaler::new(NodeId("accepted-test".into()), cfg);
    signaler.set_room_id("retry-room").await.unwrap();
    signaler.cancel_room_resubscribe();
    let event = mistlib_core::signaling::nostr::build_discovery_event(
        &signaler.codec_config,
        &signaler.crypto,
        &signaler.identity,
        "retry-room",
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let discovery = ws.next().await.unwrap().unwrap().into_text().unwrap();
        let message = ws.next().await.unwrap().unwrap().into_text().unwrap();
        assert_ne!(extract_sub_id(&discovery), extract_sub_id(&message));
        let id = extract_sub_id(&message);
        let accepted = [
            None,
            Some(serde_json::json!(["EOSE", id])),
            Some(serde_json::json!(["EVENT", id, event])),
        ];
        for acceptance in accepted {
            if let Some(acceptance) = acceptance {
                ws.send(Message::Text(acceptance.to_string().into()))
                    .await
                    .unwrap();
            }
            let closed = serde_json::json!(["CLOSED", id, "rate-limited: slow down"]);
            ws.send(Message::Text(closed.to_string().into()))
                .await
                .unwrap();
            let started = Instant::now();
            timeout(Duration::from_millis(1400), async {
                loop {
                    if let Message::Text(text) = ws.next().await.unwrap().unwrap() {
                        if text.starts_with("[\"REQ\"") {
                            assert_eq!(extract_sub_id(&text), id);
                            return;
                        }
                    }
                }
            })
            .await
            .expect("accepted subscription should restart at the 1s backoff");
            assert!(started.elapsed() >= Duration::from_millis(800));
            assert!(started.elapsed() <= Duration::from_millis(1250));
        }
    });
    signaler.connect(mpsc::channel(8).0).await.unwrap();
    let result = timeout(Duration::from_secs(5), server).await;
    signaler.close().await.unwrap();
    result.unwrap().unwrap();
}

#[tokio::test]
async fn immediate_relay_disconnects_keep_growing_reconnect_backoff() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (tx, mut rx) = mpsc::channel(8);
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            tx.send(Instant::now()).await.unwrap();
            ws.close(None).await.unwrap();
        }
    });
    let mut cfg = config();
    cfg.relays = vec![url];
    let signaler = NostrSignaler::new(NodeId("disconnect-test".into()), cfg);
    signaler.connect(mpsc::channel(8).0).await.unwrap();
    let attempts = timeout(Duration::from_secs(6), async {
        let mut attempts = Vec::new();
        for _ in 0..4 {
            attempts.push(rx.recv().await.unwrap());
        }
        attempts
    })
    .await;
    signaler.close().await.unwrap();
    server.abort();
    let attempts = attempts.expect("relay should reconnect three times");
    let delays: Vec<_> = attempts.windows(2).map(|pair| pair[1] - pair[0]).collect();
    for (delay, minimum_ms) in delays.iter().zip([400, 800, 1600]) {
        assert!(*delay >= Duration::from_millis(minimum_ms), "{delays:?}");
    }
    assert!(delays.windows(2).all(|pair| pair[1] > pair[0]));
}

#[tokio::test]
async fn hung_handshake_times_out_while_other_relay_connects_in_parallel() {
    let hung = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let healthy = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let mut cfg = config();
    // Put the stalled relay first to catch sequential connection attempts.
    cfg.relays = vec![
        format!("ws://{}", hung.local_addr().unwrap()),
        format!("ws://{}", healthy.local_addr().unwrap()),
    ];
    let (healthy_tx, healthy_rx) = tokio::sync::oneshot::channel();
    let hung_server = tokio::spawn(async move {
        let (_stream, _) = hung.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let healthy_server = tokio::spawn(async move {
        let (stream, _) = healthy.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        healthy_tx.send(()).unwrap();
        while ws.next().await.is_some() {}
    });
    let signaler = NostrSignaler::new(NodeId("parallel-test".into()), cfg);
    let connecting_signaler = signaler.clone();
    let started = Instant::now();
    let connecting =
        tokio::spawn(async move { connecting_signaler.connect(mpsc::channel(8).0).await });
    timeout(Duration::from_secs(1), healthy_rx)
        .await
        .expect("healthy relay must connect before the hung relay times out")
        .unwrap();
    let result = timeout(
        RELAY_CONNECT_TIMEOUT + Duration::from_millis(500),
        connecting,
    )
    .await;
    signaler.close().await.unwrap();
    hung_server.abort();
    healthy_server.abort();
    result
        .expect("connect must have a deadline")
        .unwrap()
        .unwrap();
    // join_all waits for every initial attempt, including the 10s timeout.
    assert!(started.elapsed() >= RELAY_CONNECT_TIMEOUT);
    assert!(started.elapsed() < RELAY_CONNECT_TIMEOUT + Duration::from_millis(500));
}

#[tokio::test]
async fn close_cancels_initial_hung_handshake() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let mut cfg = config();
    cfg.relays = vec![format!("ws://{}", listener.local_addr().unwrap())];
    let signaler = NostrSignaler::new(NodeId("cancel-test".into()), cfg);
    let connecting_signaler = signaler.clone();
    let connecting =
        tokio::spawn(async move { connecting_signaler.connect(mpsc::channel(8).0).await });
    let (_stream, _) = listener.accept().await.unwrap();
    signaler.close().await.unwrap();
    assert!(timeout(Duration::from_millis(300), connecting)
        .await
        .expect("cancel should interrupt the handshake")
        .unwrap()
        .is_err());
    assert!(signaler.senders.lock().await.is_empty());
}

#[tokio::test]
async fn initial_handshake_failure_does_not_start_supervisor() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let mut cfg = config();
    cfg.relays = vec![format!("ws://{}", listener.local_addr().unwrap())];
    let (tx, mut rx) = mpsc::channel(8);
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            tx.send(()).await.unwrap();
        }
    });
    let signaler = NostrSignaler::new(NodeId("initial-failure-test".into()), cfg);
    assert!(signaler.connect(mpsc::channel(8).0).await.is_err());
    rx.recv().await.unwrap();
    assert!(timeout(Duration::from_millis(1300), rx.recv())
        .await
        .is_err());
    signaler.close().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn close_cancels_writer_blocked_on_send() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ws = accept_async(stream).await.unwrap();
        std::future::pending::<()>().await;
    });
    let signaler = connect_room(url, false).await;
    let tx = signaler.senders.lock().await[0].clone();
    // A non-reading relay cannot drain this frame into the TCP send buffer.
    tx.send("x".repeat(16 * 1024 * 1024)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    signaler.close().await.unwrap();
    timeout(Duration::from_millis(300), tx.closed())
        .await
        .expect("cancel must drop the writer and its receiver during send");
    server.abort();
}
