use super::connection_state::SocketState;
use super::WasmNostrSignaler;
use js_sys::Uint8Array;
use mistlib_core::signaling::nostr::{
    discovery_filter, message_filter, parse_relay_message, req_frame_json, RelayMessage,
};
use mistlib_core::signaling::MessageContent;
use mistlib_core::stats::STATS;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::mpsc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{BinaryType, MessageEvent, WebSocket};
use web_time::{Duration, Instant};

const CONNECT_TIMEOUT_MS: u32 = 15_000;

type OpenWaiter = tokio::sync::oneshot::Receiver<Result<(), String>>;

pub(super) struct SocketHandlers {
    pub(super) ws: WebSocket,
    _onmessage: Closure<dyn FnMut(MessageEvent)>,
    _onopen: Closure<dyn FnMut(web_sys::Event)>,
    _onerror: Closure<dyn FnMut(web_sys::Event)>,
    _onclose: Closure<dyn FnMut(web_sys::Event)>,
}

impl SocketHandlers {
    pub(super) fn detach(self) {
        self.ws.set_onmessage(None);
        self.ws.set_onopen(None);
        self.ws.set_onerror(None);
        self.ws.set_onclose(None);
        // This may be called by one of these handlers. Keep its Closure alive
        // until the callback returns; spawn_local first polls on a microtask.
        spawn_local(async move { drop(self) });
    }
}

impl WasmNostrSignaler {
    pub async fn connect(&self, tx: mpsc::UnboundedSender<MessageContent>) -> Result<(), JsValue> {
        let epoch = self.next_reconnect_epoch();
        self.close_sockets();
        let mut cancelled = self.reconnect_cancel.subscribe();
        let mut waiters = Vec::new();
        let relays = self.resolve_relays().await?;

        for relay in &relays {
            match self.open_relay(relay.clone(), tx.clone(), epoch).await {
                Ok(connection) => waiters.push(connection),
                Err(err) => {
                    web_sys::console::warn_1(
                        &format!("WasmNostrSignaler: connect to {relay} failed: {err:?}").into(),
                    );
                }
            }
        }

        let timeout = gloo_timers::future::TimeoutFuture::new(CONNECT_TIMEOUT_MS);
        let wait_all = async {
            let mut opened_any = false;
            for (ws, waiter) in waiters {
                if matches!(waiter.await, Ok(Ok(()))) {
                    opened_any = true;
                } else {
                    self.remove_socket(&ws);
                    let _ = ws.close();
                }
            }
            opened_any
        };
        let connection = futures::future::select(Box::pin(wait_all), timeout);
        if !self.reconnect_epoch_matches(epoch) {
            return Err(JsValue::from_str(
                "WasmNostrSignaler: stale reconnect epoch",
            ));
        }
        let result = match futures::future::select(
            Box::pin(connection),
            Box::pin(cancelled.changed()),
        )
        .await
        {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(_) => {
                return Err(JsValue::from_str(
                    "WasmNostrSignaler: stale reconnect epoch",
                ))
            }
        };
        match result {
            futures::future::Either::Left((true, _)) => Ok(()),
            futures::future::Either::Left((false, _)) => Err(JsValue::from_str(
                "WasmNostrSignaler: failed to connect to any relay",
            )),
            // A hung relay must not fail the join when another one already
            // opened; sockets still connecting stay registered and subscribe
            // from onopen if they eventually open.
            futures::future::Either::Right((_, _)) if self.any_socket_open() => Ok(()),
            futures::future::Either::Right((_, _)) => Err(JsValue::from_str(
                "WasmNostrSignaler: relay connection timed out",
            )),
        }
    }

    fn any_socket_open(&self) -> bool {
        self.sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|ws| ws.ready_state() == WebSocket::OPEN)
    }

    pub(super) async fn open_relay(
        &self,
        relay: String,
        tx: mpsc::UnboundedSender<MessageContent>,
        epoch: u64,
    ) -> Result<(WebSocket, OpenWaiter), JsValue> {
        if !self.reconnect_epoch_matches(epoch) {
            return Err(JsValue::from_str(
                "WasmNostrSignaler: stale reconnect epoch",
            ));
        }

        let ws = WebSocket::new(&relay)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        // Tracks the last time any frame was received on this connection, so
        // the idle watchdog below can notice a relay that has gone silent
        // despite the app-level REQ keepalive (see `RELAY_IDLE_THRESHOLD_MS`)
        // and force a reconnect. Mirrors `mistlib-native`'s per-connection
        // `last_activity`, which native derives from Ping/Pong frames that
        // the browser `WebSocket` API does not expose to JS.
        let last_activity = Rc::new(RefCell::new(Instant::now()));

        let signaler = self.clone();
        let incoming_tx = tx.clone();
        let onmessage_last_activity = last_activity.clone();
        let onmessage_callback = Closure::wrap(Box::new(move |e: MessageEvent| {
            *onmessage_last_activity.borrow_mut() = Instant::now();
            let raw = if let Ok(ab) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = Uint8Array::new(&ab);
                match String::from_utf8(array.to_vec()) {
                    Ok(text) => text,
                    Err(err) => {
                        web_sys::console::warn_1(
                            &format!("WasmNostrSignaler: non-UTF-8 binary frame: {err}").into(),
                        );
                        return;
                    }
                }
            } else if let Some(text) = e.data().as_string() {
                text
            } else {
                return;
            };
            STATS.add_receive(raw.len() as u64);
            signaler.handle_relay_frame(&raw, &incoming_tx);
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));

        let (open_tx, open_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let open_tx = Rc::new(RefCell::new(Some(open_tx)));
        let socket_state = Rc::new(RefCell::new(SocketState::default()));

        let subscribe_signaler = self.clone();
        let subscribe_ws = ws.clone();
        let open_tx_cb = open_tx.clone();
        let opened_state = socket_state.clone();
        let opened_relay = relay.clone();
        let onopen_callback = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            if !subscribe_signaler.reconnect_epoch_matches(epoch) {
                let tx = open_tx_cb.borrow_mut().take();
                if let Some(tx) = tx {
                    let _ = tx.send(Err("stale reconnect epoch".to_string()));
                }
                return;
            }
            opened_state.borrow_mut().on_open();
            subscribe_signaler
                .reconnect_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .on_connected(&opened_relay, Instant::now());
            if let Some(room_id) = subscribe_signaler.room_id() {
                if let Err(err) = subscribe_signaler.send_subscriptions(&subscribe_ws, &room_id) {
                    let tx = open_tx_cb.borrow_mut().take();
                    if let Some(tx) = tx {
                        let _ = tx.send(Err(err.to_string()));
                    }
                    return;
                }
                if let Err(err) = subscribe_signaler.publish_discovery(&room_id) {
                    web_sys::console::warn_1(
                        &format!(
                            "WasmNostrSignaler: discovery publish after reconnect failed: {err:?}"
                        )
                        .into(),
                    );
                }
            }
            let tx = open_tx_cb.borrow_mut().take();
            if let Some(tx) = tx {
                let _ = tx.send(Ok(()));
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));

        let open_tx_cb = open_tx.clone();
        let error_ws = ws.clone();
        let onerror_callback = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            let tx = open_tx_cb.borrow_mut().take();
            if let Some(tx) = tx {
                let _ = error_ws.close();
                let _ = tx.send(Err("websocket failed to open".to_string()));
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));

        let open_tx_cb = open_tx.clone();
        let signaler = self.clone();
        let reconnect_tx = tx.clone();
        let reconnect_relay = relay.clone();
        let close_ws = ws.clone();
        let onclose_callback = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            let was_open = socket_state.borrow_mut().on_close();
            if let Some(tx) = open_tx_cb.borrow_mut().take() {
                let _ = tx.send(Err("websocket closed before opening".to_string()));
            }
            signaler.remove_socket(&close_ws);
            if !was_open || !signaler.reconnect_epoch_matches(epoch) {
                return;
            }
            signaler
                .reconnect_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .on_disconnected(&reconnect_relay, Instant::now());
            signaler.spawn_reconnect(reconnect_relay.clone(), reconnect_tx.clone(), epoch);
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        self.socket_handlers.borrow_mut().push(SocketHandlers {
            ws: ws.clone(),
            _onmessage: onmessage_callback,
            _onopen: onopen_callback,
            _onerror: onerror_callback,
            _onclose: onclose_callback,
        });

        self.spawn_idle_watchdog(ws.clone(), relay.clone(), last_activity, epoch);

        self.sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ws.clone());
        Ok((ws, open_rx))
    }

    /// Watches a single relay connection for silence and force-closes it once
    /// `RELAY_IDLE_THRESHOLD_MS` has elapsed since the last received frame,
    /// which triggers the existing `onclose` handler's reconnect path. Stops
    /// once the reconnect epoch moves on (room change, explicit `close()`, or
    /// a fresh `connect()`) or once the socket is no longer open, so it never
    /// outlives the connection it watches.
    fn spawn_idle_watchdog(
        &self,
        ws: WebSocket,
        relay: String,
        last_activity: Rc<RefCell<Instant>>,
        epoch: u64,
    ) {
        let signaler = self.clone();
        spawn_local(async move {
            loop {
                gloo_timers::future::TimeoutFuture::new(super::RELAY_KEEPALIVE_INTERVAL_MS).await;

                if !signaler.reconnect_epoch_matches(epoch) || ws.ready_state() != WebSocket::OPEN {
                    break;
                }

                let elapsed = last_activity.borrow().elapsed();
                if is_relay_idle(elapsed, super::RELAY_IDLE_THRESHOLD_MS) {
                    web_sys::console::warn_1(
                        &format!(
                            "WasmNostrSignaler: relay {relay} silent for {elapsed:?}; forcing reconnect"
                        )
                        .into(),
                    );
                    let _ = ws.close();
                    break;
                }
            }
        });
    }

    pub(super) fn close_sockets(&self) {
        let sockets = std::mem::take(&mut *self.sockets.lock().unwrap_or_else(|e| e.into_inner()));
        let handlers = std::mem::take(&mut *self.socket_handlers.borrow_mut());
        for handlers in handlers {
            handlers.detach();
        }
        for ws in sockets {
            let _ = ws.close();
        }
    }

    fn spawn_reconnect(
        &self,
        relay: String,
        tx: mpsc::UnboundedSender<MessageContent>,
        epoch: u64,
    ) {
        if !self
            .reconnect_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .claim(&relay, epoch)
        {
            return;
        }
        let signaler = self.clone();
        spawn_local(async move {
            signaler
                .reconnect_relay_loop(relay.clone(), tx, epoch)
                .await;
            signaler
                .reconnect_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .release(&relay, epoch);
        });
    }

    async fn reconnect_relay_loop(
        &self,
        relay: String,
        tx: mpsc::UnboundedSender<MessageContent>,
        epoch: u64,
    ) {
        let mut cancelled = self.reconnect_cancel.subscribe();
        while self.reconnect_epoch_matches(epoch) {
            let delay = self
                .reconnect_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .backoff(&relay)
                .next_delay();
            let delay_ms = delay.as_millis().min(u128::from(u32::MAX)) as u32;
            web_sys::console::warn_1(
                &format!(
                    "WasmNostrSignaler: relay {relay} disconnected; reconnecting in {delay:?}"
                )
                .into(),
            );
            if matches!(
                futures::future::select(
                    gloo_timers::future::TimeoutFuture::new(delay_ms),
                    Box::pin(cancelled.changed()),
                )
                .await,
                futures::future::Either::Right(_)
            ) || !self.reconnect_epoch_matches(epoch)
            {
                break;
            }
            match self.open_relay(relay.clone(), tx.clone(), epoch).await {
                Ok((ws, waiter)) => {
                    let result = self.wait_for_open(waiter, epoch, CONNECT_TIMEOUT_MS).await;
                    // onclose may run before this waiter is polled. In that
                    // case this loop still owns the relay and must continue.
                    if result.is_ok() && ws.ready_state() == WebSocket::OPEN {
                        break;
                    }
                    let message = result
                        .err()
                        .unwrap_or_else(|| "websocket closed after opening".to_string());
                    self.remove_socket(&ws);
                    let _ = ws.close();
                    if !self.reconnect_epoch_matches(epoch) {
                        break;
                    }
                    web_sys::console::warn_1(
                        &format!("WasmNostrSignaler: reconnect to {relay} failed: {message}")
                            .into(),
                    );
                }
                Err(err) => {
                    web_sys::console::warn_1(
                        &format!("WasmNostrSignaler: reconnect to {relay} failed: {err:?}").into(),
                    );
                }
            }
        }
    }

    pub(super) async fn wait_for_open(
        &self,
        waiter: OpenWaiter,
        epoch: u64,
        timeout_ms: u32,
    ) -> Result<(), String> {
        let mut cancelled = self.reconnect_cancel.subscribe();
        if !self.reconnect_epoch_matches(epoch) {
            return Err("stale reconnect epoch".to_string());
        }
        let attempt = async {
            match futures::future::select(
                waiter,
                gloo_timers::future::TimeoutFuture::new(timeout_ms),
            )
            .await
            {
                futures::future::Either::Left((result, _)) => {
                    result.unwrap_or_else(|_| Err("open waiter canceled".to_string()))
                }
                futures::future::Either::Right(_) => Err("relay connection timed out".to_string()),
            }
        };
        let result = match futures::future::select(Box::pin(attempt), Box::pin(cancelled.changed()))
            .await
        {
            futures::future::Either::Left((result, _)) if self.reconnect_epoch_matches(epoch) => {
                result
            }
            _ => Err("stale reconnect epoch".to_string()),
        };
        result
    }

    pub(super) fn subscription_frames(
        &self,
        room_id: &str,
    ) -> mistlib_core::error::Result<[String; 2]> {
        // Reuse the same subscription ids on every call (initial subscribe,
        // reconnect, and periodic keepalive re-subscribe) so a relay treats
        // a resend as a NIP-01 filter replace rather than piling up a new
        // subscription each cycle. The filters themselves are recomputed
        // fresh here, which is what keeps the accepted room-scope window
        // current across `NostrCodecConfig::room_scope_rotation_seconds`.
        let identity = self.current_identity();
        let discovery = discovery_filter(&self.codec_config, room_id);
        let message = message_filter(&self.codec_config, room_id, &identity.public_key);
        Ok([
            req_frame_json(&self.discovery_subscription_id, &[discovery])?,
            req_frame_json(&self.message_subscription_id, &[message])?,
        ])
    }

    pub(super) fn send_subscriptions(
        &self,
        ws: &WebSocket,
        room_id: &str,
    ) -> mistlib_core::error::Result<()> {
        for frame in self.subscription_frames(room_id)? {
            ws.send_with_str(&frame).map_err(|e| {
                mistlib_core::error::MistError::Network(format!(
                    "WasmNostrSignaler: subscription send failed: {:?}",
                    e
                ))
            })?;
            STATS.add_send(frame.len() as u64);
        }
        Ok(())
    }

    fn handle_relay_frame(&self, raw: &str, incoming_tx: &mpsc::UnboundedSender<MessageContent>) {
        let parsed = match parse_relay_message(raw) {
            Ok(Some(parsed)) => parsed,
            Ok(None) => return,
            Err(err) => {
                web_sys::console::warn_1(
                    &format!("WasmNostrSignaler: relay frame parse failed: {err:?}").into(),
                );
                return;
            }
        };
        self.handle_relay_message(parsed, incoming_tx);
    }

    fn handle_relay_message(
        &self,
        message: RelayMessage,
        incoming_tx: &mpsc::UnboundedSender<MessageContent>,
    ) {
        match message {
            RelayMessage::Event { event, .. } => {
                if let Err(err) = self.process_event(event, incoming_tx) {
                    web_sys::console::warn_1(
                        &format!("WasmNostrSignaler: event processing failed: {err:?}").into(),
                    );
                }
            }
            status => log_relay_status(status),
        }
    }
}

/// Pure predicate behind the idle watchdog: has `elapsed` (time since the
/// connection last received any frame) reached `threshold_ms`?
pub(super) fn is_relay_idle(elapsed: Duration, threshold_ms: u32) -> bool {
    elapsed >= Duration::from_millis(u64::from(threshold_ms))
}

fn log_relay_status(message: RelayMessage) {
    match message {
        RelayMessage::Ok {
            event_id,
            accepted,
            message,
        } => {
            if !accepted {
                web_sys::console::warn_1(
                    &format!("WasmNostrSignaler: relay rejected event {event_id}: {message}")
                        .into(),
                );
            }
        }
        RelayMessage::Notice(message) => {
            web_sys::console::warn_1(&format!("WasmNostrSignaler: relay notice: {message}").into());
        }
        RelayMessage::Closed {
            subscription_id,
            message,
        } => {
            web_sys::console::warn_1(
                &format!(
                    "WasmNostrSignaler: relay closed subscription {subscription_id}: {message}"
                )
                .into(),
            );
        }
        RelayMessage::Auth(challenge) => {
            web_sys::console::warn_1(
                &format!(
                    "WasmNostrSignaler: relay requested AUTH challenge {challenge}; NIP-42 auth is not implemented"
                )
                .into(),
            );
        }
        RelayMessage::Eose { .. } | RelayMessage::Event { .. } => {}
    }
}
