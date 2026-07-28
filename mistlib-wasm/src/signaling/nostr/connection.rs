use super::WasmNostrSignaler;
use js_sys::Uint8Array;
use mistlib_core::signaling::nostr::{
    discovery_filter, message_filter, parse_relay_message, req_frame_json, RelayMessage,
};
use mistlib_core::signaling::reconnect::random_reconnect_backoff_delay;
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

impl WasmNostrSignaler {
    pub async fn connect(&self, tx: mpsc::UnboundedSender<MessageContent>) -> Result<(), JsValue> {
        self.next_reconnect_epoch();
        self.sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let epoch = self.next_reconnect_epoch();
        let mut waiters = Vec::new();
        let relays = self.resolve_relays().await?;

        for relay in &relays {
            match self.open_relay(relay.clone(), tx.clone(), epoch).await {
                Ok(waiter) => waiters.push(waiter),
                Err(err) => {
                    web_sys::console::warn_1(
                        &format!("WasmNostrSignaler: connect to {relay} failed: {err:?}").into(),
                    );
                }
            }
        }

        let timeout = gloo_timers::future::TimeoutFuture::new(CONNECT_TIMEOUT_MS);
        let wait_all = async move {
            let mut opened_any = false;
            for waiter in waiters {
                if matches!(waiter.await, Ok(Ok(()))) {
                    opened_any = true;
                }
            }
            opened_any
        };

        match futures::future::select(Box::pin(wait_all), timeout).await {
            futures::future::Either::Left((true, _)) => Ok(()),
            futures::future::Either::Left((false, _)) => Err(JsValue::from_str(
                "WasmNostrSignaler: failed to connect to any relay",
            )),
            futures::future::Either::Right((_, _)) => Err(JsValue::from_str(
                "WasmNostrSignaler: relay connection timed out",
            )),
        }
    }

    async fn open_relay(
        &self,
        relay: String,
        tx: mpsc::UnboundedSender<MessageContent>,
        epoch: u64,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<(), String>>, JsValue> {
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
        onmessage_callback.forget();

        let (open_tx, open_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let open_tx = Rc::new(RefCell::new(Some(open_tx)));

        let subscribe_signaler = self.clone();
        let subscribe_ws = ws.clone();
        let open_tx_cb = open_tx.clone();
        let onopen_callback = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            if !subscribe_signaler.reconnect_epoch_matches(epoch) {
                if let Some(tx) = open_tx_cb.borrow_mut().take() {
                    let _ = tx.send(Err("stale reconnect epoch".to_string()));
                }
                return;
            }
            if let Some(room_id) = subscribe_signaler.room_id() {
                if let Err(err) = subscribe_signaler.send_subscriptions(&subscribe_ws, &room_id) {
                    if let Some(tx) = open_tx_cb.borrow_mut().take() {
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
            if let Some(tx) = open_tx_cb.borrow_mut().take() {
                let _ = tx.send(Ok(()));
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        onopen_callback.forget();

        let open_tx_cb = open_tx.clone();
        let error_signaler = self.clone();
        let error_ws = ws.clone();
        let onerror_callback = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            if let Some(tx) = open_tx_cb.borrow_mut().take() {
                error_signaler.remove_socket(&error_ws);
                let _ = error_ws.close();
                let _ = tx.send(Err("websocket failed to open".to_string()));
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
        onerror_callback.forget();

        let open_tx_cb = open_tx.clone();
        let signaler = self.clone();
        let reconnect_tx = tx.clone();
        let reconnect_relay = relay.clone();
        let close_ws = ws.clone();
        let onclose_callback = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            if let Some(tx) = open_tx_cb.borrow_mut().take() {
                signaler.remove_socket(&close_ws);
                let _ = tx.send(Err("websocket closed before opening".to_string()));
                return;
            }
            if !signaler.reconnect_epoch_matches(epoch) {
                return;
            }
            signaler.remove_socket(&close_ws);
            let signaler = signaler.clone();
            let reconnect_tx = reconnect_tx.clone();
            let reconnect_relay = reconnect_relay.clone();
            spawn_local(async move {
                signaler
                    .reconnect_relay_loop(reconnect_relay, reconnect_tx, epoch)
                    .await;
            });
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        onclose_callback.forget();

        self.spawn_idle_watchdog(ws.clone(), relay.clone(), last_activity, epoch);

        self.sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ws);
        Ok(open_rx)
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

    async fn reconnect_relay_loop(
        &self,
        relay: String,
        tx: mpsc::UnboundedSender<MessageContent>,
        epoch: u64,
    ) {
        let mut attempt = 0_u32;
        while self.reconnect_epoch_matches(epoch) {
            let delay = random_reconnect_backoff_delay(attempt);
            let delay_ms = delay.as_millis().min(u128::from(u32::MAX)) as u32;
            web_sys::console::warn_1(
                &format!(
                    "WasmNostrSignaler: relay {relay} disconnected; reconnecting in {:?}",
                    delay
                )
                .into(),
            );
            gloo_timers::future::TimeoutFuture::new(delay_ms).await;
            if !self.reconnect_epoch_matches(epoch) {
                break;
            }
            match self.open_relay(relay.clone(), tx.clone(), epoch).await {
                Ok(waiter) => match waiter.await {
                    Ok(Ok(())) => break,
                    Ok(Err(message)) => {
                        web_sys::console::warn_1(
                            &format!("WasmNostrSignaler: reconnect to {relay} failed: {message}")
                                .into(),
                        );
                    }
                    Err(_) => {
                        web_sys::console::warn_1(
                            &format!(
                                "WasmNostrSignaler: reconnect to {relay} open waiter canceled"
                            )
                            .into(),
                        );
                    }
                },
                Err(err) => {
                    web_sys::console::warn_1(
                        &format!("WasmNostrSignaler: reconnect to {relay} failed: {err:?}").into(),
                    );
                }
            }
            attempt = attempt.saturating_add(1);
        }
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
