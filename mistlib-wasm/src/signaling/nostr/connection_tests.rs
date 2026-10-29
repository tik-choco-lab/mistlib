use super::connection_state::{ReconnectState, SocketState};
use super::WasmNostrSignaler;
use futures::future::{select, Either};
use gloo_timers::future::TimeoutFuture;
use mistlib_core::config::NostrSignalingConfig;
use mistlib_core::signaling::reconnect::STABLE_CONNECTION_RESET;
use mistlib_core::signaling::Signaler;
use mistlib_core::types::NodeId;
use std::rc::Rc;
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen::prelude::*;
use wasm_bindgen_test::wasm_bindgen_test;
use web_time::{Duration, Instant};

#[wasm_bindgen_test]
fn initial_open_failures_never_start_reconnect() {
    let mut reconnect = ReconnectState::default();
    reconnect.reset(1);
    let mut loops = 0;
    for _ in 0..64 {
        let mut socket = SocketState::default();
        // onerror consumes the open waiter, but does not mark the socket open.
        if socket.on_close() && reconnect.claim("relay", 1) {
            loops += 1;
        }
    }
    assert_eq!(loops, 0);
}

#[wasm_bindgen_test]
fn repeated_open_failures_keep_one_loop_and_grow_backoff() {
    let mut reconnect = ReconnectState::default();
    reconnect.reset(1);
    let mut socket = SocketState::default();
    socket.on_open();
    assert!(socket.on_close());
    assert!(reconnect.claim("relay", 1));
    let mut loops = 1;
    let mut delays = Vec::new();
    for _ in 0..64 {
        delays.push(
            reconnect
                .backoff("relay")
                .next_delay_with_jitter(0.0)
                .as_millis(),
        );
        let mut failed_socket = SocketState::default();
        if failed_socket.on_close() && reconnect.claim("relay", 1) {
            loops += 1;
        }
        // Even a duplicate reconnect request cannot create a second loop.
        assert!(!reconnect.claim("relay", 1));
    }
    assert_eq!(loops, 1);
    assert_eq!(
        &delays[..8],
        &[500, 1000, 2000, 4000, 8000, 16000, 30000, 30000]
    );
    assert!(delays[8..].iter().all(|delay| *delay == 30000));
}

#[wasm_bindgen_test]
fn reconnect_claims_are_per_relay_and_epoch() {
    let mut reconnect = ReconnectState::default();
    reconnect.reset(1);
    assert!(reconnect.claim("a", 1));
    assert!(reconnect.claim("b", 1));
    assert!(!reconnect.claim("a", 1));
    reconnect.backoff("a").next_delay_with_jitter(0.0);
    reconnect.reset(2);
    assert!(!reconnect.claim("a", 1));
    assert!(reconnect.claim("a", 2));
    reconnect.release("a", 1);
    assert!(
        !reconnect.claim("a", 2),
        "old loop released a new epoch's claim"
    );
    assert_eq!(reconnect.backoff("a").attempt(), 0);
}

#[wasm_bindgen_test]
fn short_connections_preserve_backoff_between_loops() {
    let mut reconnect = ReconnectState::default();
    reconnect.reset(1);
    let now = Instant::now();
    for expected in [500, 1000, 2000, 4000] {
        assert!(reconnect.claim("relay", 1));
        assert_eq!(
            reconnect
                .backoff("relay")
                .next_delay_with_jitter(0.0)
                .as_millis(),
            expected
        );
        reconnect.on_connected("relay", now);
        reconnect.release("relay", 1);
        reconnect.on_disconnected("relay", now + Duration::from_secs(1));
    }
    assert_eq!(reconnect.backoff("other").attempt(), 0);
}

#[wasm_bindgen_test]
fn stable_connection_resets_backoff_and_close_is_consumed_once() {
    let mut reconnect = ReconnectState::default();
    reconnect.reset(1);
    reconnect.backoff("relay").next_delay_with_jitter(0.0);
    let now = Instant::now();
    let mut socket = SocketState::default();
    socket.on_open();
    reconnect.on_connected("relay", now);
    assert!(socket.on_close());
    reconnect.on_disconnected("relay", now + STABLE_CONNECTION_RESET);
    assert!(!socket.on_close());
    assert_eq!(
        reconnect
            .backoff("relay")
            .next_delay_with_jitter(0.0)
            .as_millis(),
        500
    );
}

#[wasm_bindgen(inline_js = "
let originalWebSocket;
export function installRelaySocketMock() {
    originalWebSocket = globalThis.WebSocket;
    globalThis.WebSocket = class {
        constructor(url) { this.url = url; this.readyState = 0; }
        close() { this.readyState = 2; }
        send() {}
    };
}
export function restoreRelaySocketMock() { globalThis.WebSocket = originalWebSocket; }
export function emitRelayOpen(ws) { ws.readyState = 1; ws.onopen({}); }
export function emitRelayClose(ws) { ws.readyState = 3; if (ws.onclose) ws.onclose({}); }
export function emitRelayErrorClose(ws) {
    ws.onerror({});
    ws.readyState = 3;
    if (ws.onclose) ws.onclose({});
}
export function relayHandlersDetached(ws) {
    return !ws.onmessage && !ws.onopen && !ws.onerror && !ws.onclose;
}
")]
extern "C" {
    fn installRelaySocketMock();
    fn restoreRelaySocketMock();
    fn emitRelayOpen(ws: &web_sys::WebSocket);
    fn emitRelayErrorClose(ws: &web_sys::WebSocket);
    fn emitRelayClose(ws: &web_sys::WebSocket);
    fn relayHandlersDetached(ws: &web_sys::WebSocket) -> bool;
}

struct SocketMock;
impl SocketMock {
    fn install() -> Self {
        installRelaySocketMock();
        Self
    }
}
impl Drop for SocketMock {
    fn drop(&mut self) {
        restoreRelaySocketMock();
    }
}

fn signaler() -> WasmNostrSignaler {
    WasmNostrSignaler::new(
        NodeId("reconnect-test".into()),
        NostrSignalingConfig::default(),
    )
}

#[wasm_bindgen_test(async)]
async fn error_then_close_detaches_and_drops_handlers_without_reconnect() {
    let _mock = SocketMock::install();
    let signaler = signaler();
    let epoch = signaler.next_reconnect_epoch();
    let baseline = Rc::strong_count(&signaler.socket_handlers);
    let (tx, _rx) = mpsc::unbounded_channel();
    let (ws, waiter) = signaler
        .open_relay("ws://relay".into(), tx, epoch)
        .await
        .unwrap();
    emitRelayErrorClose(&ws);
    assert!(waiter.await.unwrap().is_err());
    assert!(relayHandlersDetached(&ws));
    assert!(signaler.socket_handlers.borrow().is_empty());
    assert!(signaler.sockets.lock().unwrap().is_empty());
    assert!(signaler
        .reconnect_state
        .lock()
        .unwrap()
        .claim("ws://relay", epoch));
    signaler
        .reconnect_state
        .lock()
        .unwrap()
        .release("ws://relay", epoch);
    // Include the per-socket watchdog's first wakeup before checking ownership.
    TimeoutFuture::new(super::RELAY_KEEPALIVE_INTERVAL_MS + 50).await;
    assert_eq!(Rc::strong_count(&signaler.socket_handlers), baseline);
    signaler.close().await.unwrap();
}

#[wasm_bindgen_test(async)]
async fn explicit_close_detaches_open_and_connecting_socket_handlers() {
    let _mock = SocketMock::install();
    let signaler = signaler();
    let epoch = signaler.next_reconnect_epoch();
    let (tx, _rx) = mpsc::unbounded_channel();
    let (open, opened) = signaler
        .open_relay("ws://open".into(), tx.clone(), epoch)
        .await
        .unwrap();
    let (connecting, pending) = signaler
        .open_relay("ws://pending".into(), tx, epoch)
        .await
        .unwrap();
    emitRelayOpen(&open);
    assert!(opened.await.unwrap().is_ok());
    signaler.close().await.unwrap();
    assert!(relayHandlersDetached(&open));
    assert!(relayHandlersDetached(&connecting));
    TimeoutFuture::new(0).await;
    assert!(pending.await.is_err());
    assert!(signaler.socket_handlers.borrow().is_empty());
    assert!(!signaler
        .reconnect_state
        .lock()
        .unwrap()
        .claim("ws://open", epoch));
}

#[wasm_bindgen_test(async)]
async fn reconnect_open_wait_has_a_deadline() {
    let signaler = signaler();
    let epoch = signaler.next_reconnect_epoch();
    let (_tx, waiter) = oneshot::channel();
    assert_eq!(
        signaler.wait_for_open(waiter, epoch, 1).await,
        Err("relay connection timed out".into())
    );
    signaler.close().await.unwrap();
}

#[wasm_bindgen_test(async)]
async fn epoch_change_interrupts_reconnect_open_wait() {
    let signaler = signaler();
    let epoch = signaler.next_reconnect_epoch();
    let (_tx, waiter) = oneshot::channel();
    let change_epoch = async {
        TimeoutFuture::new(1).await;
        signaler.close().await.unwrap();
    };
    let wait = signaler.wait_for_open(waiter, epoch, 15_000);
    let (result, ()) = futures::join!(wait, change_epoch);
    assert_eq!(result, Err("stale reconnect epoch".into()));
    // A waiter created after cancellation must also return immediately.
    let (_tx, waiter) = oneshot::channel();
    assert!(matches!(
        select(
            Box::pin(signaler.wait_for_open(waiter, epoch, 15_000)),
            TimeoutFuture::new(100)
        )
        .await,
        Either::Left((Err(_), _))
    ));
}

#[wasm_bindgen_test(async)]
async fn reconnect_error_close_and_immediate_disconnect_keep_single_owner() {
    let _mock = SocketMock::install();
    let signaler = signaler();
    let epoch = signaler.next_reconnect_epoch();
    let relay = "ws://retry";
    let (tx, _rx) = mpsc::unbounded_channel();
    let (ws, waiter) = signaler.open_relay(relay.into(), tx, epoch).await.unwrap();
    emitRelayOpen(&ws);
    waiter.await.unwrap().unwrap();
    emitRelayClose(&ws);

    for attempt in 1..=3 {
        let next_socket = async {
            loop {
                if let Some(ws) = signaler.sockets.lock().unwrap().first().cloned() {
                    break ws;
                }
                TimeoutFuture::new(10).await;
            }
        };
        let ws = match select(Box::pin(next_socket), TimeoutFuture::new(3000)).await {
            Either::Left((ws, _)) => ws,
            Either::Right(_) => panic!("reconnect did not create its next socket"),
        };
        {
            let mut state = signaler.reconnect_state.lock().unwrap();
            assert!(!state.claim(relay, epoch));
            assert_eq!(state.backoff(relay).attempt(), attempt);
        }
        if attempt == 2 {
            // Both events arrive before the open waiter resumes: ownership
            // must stay with the existing loop until its next failed attempt.
            emitRelayOpen(&ws);
            emitRelayClose(&ws);
        } else {
            emitRelayErrorClose(&ws);
        }
        assert!(relayHandlersDetached(&ws));
        TimeoutFuture::new(0).await;
        assert_eq!(
            signaler
                .reconnect_state
                .lock()
                .unwrap()
                .backoff(relay)
                .attempt(),
            attempt + 1
        );
        assert!(signaler.sockets.lock().unwrap().is_empty());
    }
    signaler.close().await.unwrap();
    TimeoutFuture::new(0).await;
    assert!(signaler.socket_handlers.borrow().is_empty());
}
