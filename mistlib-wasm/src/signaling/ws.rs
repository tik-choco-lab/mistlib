use async_trait::async_trait;
use js_sys::Uint8Array;
use mistlib_core::signaling::reconnect::random_reconnect_backoff_delay;
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData};
use mistlib_core::stats::STATS;
use mistlib_core::types::{NodeId, SessionReestablishedHook};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{BinaryType, MessageEvent, WebSocket};

const CONNECT_TIMEOUT_MS: u32 = 15_000;

/// Forwards a decoded wire `SignalingData` to the local incoming channel,
/// dropping it instead if it is locally-synthesized-only (currently only
/// `SignalingType::Rejoin`; see `SignalingType::is_local_only`'s doc
/// comment).
///
/// Unlike `mistlib-wasm`'s Nostr signaler, plain WebSocket signaling frames
/// carry no per-message signing or encryption: any signaling server (or a
/// MITM on the connection, since this is legacy/fallback plaintext
/// signaling) can hand this process any JSON it likes. Without this guard, a
/// hostile/compromised server could inject a crafted `Rejoin` naming an
/// arbitrary `NodeId` and force `WasmWebRtcTransport::handle_message` to
/// tear down that peer's live connection on demand -- repeatable at will as
/// a denial-of-service. `Rejoin` must only ever be synthesized locally by
/// this process's own signaling layer, never accepted from the wire.
fn forward_incoming_signaling_data(
    tx: &mpsc::UnboundedSender<MessageContent>,
    data: SignalingData,
) {
    if data.signaling_type.is_local_only() {
        web_sys::console::warn_1(
            &format!(
                "WasmWebSocketSignaler: dropping wire-delivered {:?} from {} -- this signaling \
                 type is local-only and must never arrive from the wire",
                data.signaling_type, data.sender_id
            )
            .into(),
        );
        return;
    }
    if tx.send(MessageContent::Data(data)).is_err() {
        web_sys::console::warn_1(&"WasmWebSocketSignaler: signaling receiver dropped".into());
    }
}

#[derive(Clone)]
pub struct WasmWebSocketSignaler {
    url: String,
    socket: Arc<Mutex<Option<WebSocket>>>,
    incoming_tx: Arc<Mutex<Option<mpsc::UnboundedSender<MessageContent>>>>,
    session_epoch: Arc<Mutex<u64>>,
    reset_inflight: Arc<Mutex<bool>>,
    on_session_reestablished: Arc<Mutex<Option<SessionReestablishedHook>>>,
}

impl WasmWebSocketSignaler {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            socket: Arc::new(Mutex::new(None)),
            incoming_tx: Arc::new(Mutex::new(None)),
            session_epoch: Arc::new(Mutex::new(0)),
            reset_inflight: Arc::new(Mutex::new(false)),
            on_session_reestablished: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn connect(&self, tx: mpsc::UnboundedSender<MessageContent>) -> Result<(), JsValue> {
        *self.incoming_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
        let epoch = self.next_epoch();
        if let Some(ws) = self.socket.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = ws.close();
        }
        self.open_socket(tx, epoch, false).await
    }

    async fn open_socket(
        &self,
        tx: mpsc::UnboundedSender<MessageContent>,
        epoch: u64,
        notify_reestablished: bool,
    ) -> Result<(), JsValue> {
        if !self.is_epoch_current(epoch) {
            return Err(JsValue::from_str(
                "WasmWebSocketSignaler: stale reconnect epoch",
            ));
        }

        let ws = WebSocket::new(&self.url)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let message_tx = tx.clone();
        let onmessage_callback = Closure::wrap(Box::new(move |e: MessageEvent| {
            if let Ok(ab) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = Uint8Array::new(&ab);
                let vec = array.to_vec();
                STATS.add_receive(vec.len() as u64);
                match serde_json::from_slice::<SignalingData>(&vec) {
                    Ok(data) => forward_incoming_signaling_data(&message_tx, data),
                    Err(err) => {
                        web_sys::console::warn_1(
                            &format!(
                                "WasmWebSocketSignaler: failed to decode binary signaling payload: {}",
                                err
                            )
                            .into(),
                        );
                    }
                }
            } else if let Some(txt) = e.data().as_string() {
                STATS.add_receive(txt.len() as u64);
                match serde_json::from_str::<SignalingData>(&txt) {
                    Ok(data) => forward_incoming_signaling_data(&message_tx, data),
                    Err(err) => {
                        web_sys::console::warn_1(
                            &format!(
                                "WasmWebSocketSignaler: failed to decode text signaling payload: {}",
                                err
                            )
                            .into(),
                        );
                    }
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);

        ws.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
        onmessage_callback.forget();

        let (open_tx, open_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let open_tx = Rc::new(RefCell::new(Some(open_tx)));

        let open_tx_cb = open_tx.clone();
        let onerror_callback = Closure::wrap(Box::new(move |e: web_sys::Event| {
            web_sys::console::error_1(&e);
            if let Some(tx) = open_tx_cb.borrow_mut().take() {
                let _ = tx.send(Err("websocket failed to open".to_string()));
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
        onerror_callback.forget();

        let open_tx_cb = open_tx.clone();
        let onopen_callback = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            if let Some(tx) = open_tx_cb.borrow_mut().take() {
                let _ = tx.send(Ok(()));
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        onopen_callback.forget();

        let open_tx_cb = open_tx.clone();
        let signaler = self.clone();
        let reconnect_tx = tx.clone();
        let reconnect_ws = ws.clone();
        let onclose_callback = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            if let Some(tx) = open_tx_cb.borrow_mut().take() {
                let _ = tx.send(Err("websocket closed before opening".to_string()));
                return;
            }
            if !signaler.is_epoch_current(epoch) {
                return;
            }
            signaler.clear_socket_if_same(&reconnect_ws);
            let signaler = signaler.clone();
            let reconnect_tx = reconnect_tx.clone();
            spawn_local(async move {
                signaler.reconnect_loop(reconnect_tx, epoch).await;
            });
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        onclose_callback.forget();

        {
            let mut lock = self.socket.lock().unwrap_or_else(|e| e.into_inner());
            *lock = Some(ws.clone());
        }

        let timeout = gloo_timers::future::TimeoutFuture::new(CONNECT_TIMEOUT_MS);
        let result = match futures::future::select(open_rx, timeout).await {
            futures::future::Either::Left((Ok(Ok(())), _)) => Ok(()),
            futures::future::Either::Left((Ok(Err(message)), _)) => Err(JsValue::from_str(
                &format!("WasmWebSocketSignaler: {}", message),
            )),
            futures::future::Either::Left((Err(_), _)) => Err(JsValue::from_str(
                "WasmWebSocketSignaler: websocket open waiter was canceled",
            )),
            futures::future::Either::Right((_, _)) => Err(JsValue::from_str(
                "WasmWebSocketSignaler: websocket connection timed out",
            )),
        };

        if result.is_err() || !self.is_epoch_current(epoch) {
            ws.set_onmessage(None);
            ws.set_onerror(None);
            ws.set_onopen(None);
            ws.set_onclose(None);
            let _ = ws.close();
            self.clear_socket_if_same(&ws);
        } else if notify_reestablished {
            self.call_reestablished_hook();
        }

        result
    }

    async fn reconnect_loop(&self, tx: mpsc::UnboundedSender<MessageContent>, epoch: u64) {
        let mut attempt = 0_u32;
        while self.is_epoch_current(epoch) {
            let delay = random_reconnect_backoff_delay(attempt);
            let delay_ms = delay.as_millis().min(u128::from(u32::MAX)) as u32;
            web_sys::console::warn_1(
                &format!(
                    "WasmWebSocketSignaler: disconnected; reconnecting in {:?}",
                    delay
                )
                .into(),
            );
            gloo_timers::future::TimeoutFuture::new(delay_ms).await;
            if !self.is_epoch_current(epoch) {
                break;
            }
            match self.open_socket(tx.clone(), epoch, true).await {
                Ok(()) => break,
                Err(err) => {
                    web_sys::console::warn_1(
                        &format!("WasmWebSocketSignaler: reconnect failed: {err:?}").into(),
                    );
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }

    async fn reset_session_impl(&self) -> mistlib_core::error::Result<()> {
        let Some(tx) = self
            .incoming_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return Ok(());
        };

        tracing::info!("WasmWebSocketSignaler: resetting signaling session");
        let epoch = self.next_epoch();
        if let Some(ws) = self.socket.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = ws.close();
        }

        match self.open_socket(tx.clone(), epoch, true).await {
            Ok(()) => Ok(()),
            Err(err) => {
                let signaler = self.clone();
                spawn_local(async move {
                    signaler.reconnect_loop(tx, epoch).await;
                });
                Err(mistlib_core::error::MistError::Network(format!(
                    "WasmWebSocketSignaler: reset reconnect failed: {:?}",
                    err
                )))
            }
        }
    }

    fn next_epoch(&self) -> u64 {
        let mut epoch = self.session_epoch.lock().unwrap_or_else(|e| e.into_inner());
        *epoch = epoch.saturating_add(1);
        *epoch
    }

    fn is_epoch_current(&self, epoch: u64) -> bool {
        *self.session_epoch.lock().unwrap_or_else(|e| e.into_inner()) == epoch
    }

    fn clear_socket_if_same(&self, ws: &WebSocket) {
        let mut lock = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        if lock
            .as_ref()
            .map(|current| js_sys::Object::is(current.as_ref(), ws.as_ref()))
            .unwrap_or(false)
        {
            *lock = None;
        }
    }

    fn call_reestablished_hook(&self) {
        let hook = self
            .on_session_reestablished
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Signaler for WasmWebSocketSignaler {
    async fn send_signaling(
        &self,
        _to: &NodeId,
        msg: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        // `Rejoin` is synthesized locally by the signaling layer purely to
        // notify this process's own transport (see `SignalingType::Rejoin`'s
        // doc comment) and must never be sent over the wire.
        if let MessageContent::Data(data) = &msg {
            if data.signaling_type.is_local_only() {
                return Ok(());
            }
        }
        let ws_opt = {
            let lock = self.socket.lock().unwrap_or_else(|e| e.into_inner());
            lock.as_ref().cloned()
        };
        if let Some(ws) = ws_opt {
            if ws.ready_state() == WebSocket::OPEN {
                let json = match msg {
                    MessageContent::Data(data) => serde_json::to_string(&data).map_err(|e| {
                        mistlib_core::error::MistError::Serialization(e.to_string())
                    })?,
                    _ => {
                        return Err(mistlib_core::error::MistError::Signaling(
                            "WasmWebSocketSignaler: unsupported message type".to_string(),
                        ))
                    }
                };
                ws.send_with_str(&json).map_err(|e| {
                    mistlib_core::error::MistError::Network(format!(
                        "WasmWebSocketSignaler: websocket send failed: {:?}",
                        e
                    ))
                })?;
                STATS.add_send(json.len() as u64);
                return Ok(());
            }
        }
        Err(mistlib_core::error::MistError::Signaling(
            "WasmWebSocketSignaler: websocket is not open".to_string(),
        ))
    }

    async fn reset_session(&self) -> mistlib_core::error::Result<()> {
        {
            let mut inflight = self
                .reset_inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if *inflight {
                return Ok(());
            }
            *inflight = true;
        }

        let result = self.reset_session_impl().await;
        *self
            .reset_inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = false;
        result
    }

    fn set_on_session_reestablished(&self, hook: SessionReestablishedHook) {
        *self
            .on_session_reestablished
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    async fn close(&self) -> mistlib_core::error::Result<()> {
        *self.incoming_tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.next_epoch();
        let mut lock = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ws) = lock.take() {
            ws.close().map_err(|e| {
                mistlib_core::error::MistError::Network(format!(
                    "WasmWebSocketSignaler: websocket close failed: {:?}",
                    e
                ))
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mistlib_core::signaling::SignalingType;
    use wasm_bindgen_test::wasm_bindgen_test;

    // `ws.rs` has no harness for driving an actual `WebSocket` end-to-end
    // (unlike `signaling/nostr.rs`, whose `process_event` is a directly
    // callable method exercised in `nostr/tests.rs`, or
    // `mistlib-native/src/signaling/ws.rs`, which spins up a real TCP
    // WebSocket server in-process). `forward_incoming_signaling_data` is the
    // pure chokepoint both the binary and text `onmessage` decode branches
    // route through, so it is exercised directly here instead.

    #[wasm_bindgen_test]
    fn wire_delivered_rejoin_is_dropped_not_forwarded() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rejoin = SignalingData {
            sender_id: NodeId("wasm-bob".to_string()),
            receiver_id: NodeId("wasm-alice".to_string()),
            room_id: "room-a".to_string(),
            data: "999".to_string(),
            signaling_type: SignalingType::Rejoin,
        };

        forward_incoming_signaling_data(&tx, rejoin);

        assert!(
            rx.try_recv().is_err(),
            "a wire-delivered Rejoin must never reach the incoming signaling channel"
        );
    }

    #[wasm_bindgen_test]
    fn normal_signaling_payload_is_still_forwarded() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let offer = SignalingData {
            sender_id: NodeId("wasm-bob".to_string()),
            receiver_id: NodeId("wasm-alice".to_string()),
            room_id: "room-a".to_string(),
            data: "v=0\r\ns=session".to_string(),
            signaling_type: SignalingType::Offer,
        };

        forward_incoming_signaling_data(&tx, offer.clone());

        match rx.try_recv().unwrap() {
            MessageContent::Data(data) => assert_eq!(data, offer),
            other => panic!("unexpected signaling message: {other:?}"),
        }
    }
}
