use async_trait::async_trait;
use js_sys::Uint8Array;
use mistlib_core::signaling::{MessageContent, Signaler};
use mistlib_core::types::NodeId;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, MessageEvent, WebSocket};

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
                if let Ok(data) =
                    serde_json::from_slice::<mistlib_core::signaling::SignalingData>(&vec)
                {
                    let _ = tx.send(MessageContent::Data(data));
                }
            } else if let Some(txt) = e.data().as_string() {
                if let Ok(data) =
                    serde_json::from_str::<mistlib_core::signaling::SignalingData>(&txt)
                {
                    let _ = tx.send(MessageContent::Data(data));
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);

        ws.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
        onmessage_callback.forget();

        let onerror_callback = Closure::wrap(Box::new(move |e: web_sys::Event| {
            web_sys::console::error_1(&e);
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
        onerror_callback.forget();

        let (open_tx, open_rx) = tokio::sync::oneshot::channel::<()>();
        let mut open_tx = Some(open_tx);
        let onopen_callback = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            if let Some(tx) = open_tx.take() {
                let _ = tx.send(());
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        ws.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        onopen_callback.forget();

        let mut lock = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        *lock = Some(ws);
        drop(lock);

        let _ = open_rx.await;
        Ok(())
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
        let lock = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ws) = lock.as_ref() {
            if ws.ready_state() == WebSocket::OPEN {
                let json = match msg {
                    MessageContent::Data(data) => serde_json::to_string(&data)
                        .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?,
                    _ => {
                        return Err(mistlib_core::error::MistError::Internal(
                            "Unsupported".to_string(),
                        ))
                    }
                };
                ws.send_with_str(&json)
                    .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;
                return Ok(());
            }
        }
        Err(mistlib_core::error::MistError::Internal(
            "WS not open".to_string(),
        ))
    }
}
