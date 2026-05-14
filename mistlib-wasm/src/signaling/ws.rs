use async_trait::async_trait;
use js_sys::Uint8Array;
use mistlib_core::signaling::{MessageContent, Signaler};
use mistlib_core::stats::STATS;
use mistlib_core::types::NodeId;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, MessageEvent, WebSocket};

const CONNECT_TIMEOUT_MS: u32 = 15_000;

pub struct WasmWebSocketSignaler {
    url: String,
    socket: Arc<Mutex<Option<WebSocket>>>,
}

impl WasmWebSocketSignaler {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            socket: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn connect(&self, tx: mpsc::UnboundedSender<MessageContent>) -> Result<(), JsValue> {
        let ws = WebSocket::new(&self.url)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let onmessage_callback = Closure::wrap(Box::new(move |e: MessageEvent| {
            if let Ok(ab) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = Uint8Array::new(&ab);
                let vec = array.to_vec();
                STATS.add_receive(vec.len() as u64);
                match serde_json::from_slice::<mistlib_core::signaling::SignalingData>(&vec) {
                    Ok(data) => {
                        if tx.send(MessageContent::Data(data)).is_err() {
                            web_sys::console::warn_1(
                                &"WasmWebSocketSignaler: signaling receiver dropped".into(),
                            );
                        }
                    }
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
                match serde_json::from_str::<mistlib_core::signaling::SignalingData>(&txt) {
                    Ok(data) => {
                        if tx.send(MessageContent::Data(data)).is_err() {
                            web_sys::console::warn_1(
                                &"WasmWebSocketSignaler: signaling receiver dropped".into(),
                            );
                        }
                    }
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
        let onclose_callback = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            if let Some(tx) = open_tx_cb.borrow_mut().take() {
                let _ = tx.send(Err("websocket closed before opening".to_string()));
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        onclose_callback.forget();

        let mut lock = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        *lock = Some(ws.clone());
        drop(lock);

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

        if result.is_err() {
            ws.set_onmessage(None);
            ws.set_onerror(None);
            ws.set_onopen(None);
            ws.set_onclose(None);
            let _ = ws.close();
            let mut lock = self.socket.lock().unwrap_or_else(|e| e.into_inner());
            *lock = None;
        }

        result
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

    async fn close(&self) -> mistlib_core::error::Result<()> {
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
