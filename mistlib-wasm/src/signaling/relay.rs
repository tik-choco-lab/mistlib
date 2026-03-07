use async_trait::async_trait;
use mistlib_core::signaling::{MessageContent, Signaler};
use mistlib_core::types::NodeId;
use std::sync::{Arc, Mutex};

pub struct SignalingRelay {
    pub websocket: Arc<Mutex<Option<Arc<dyn Signaler>>>>,
    pub overlay: Arc<Mutex<Option<Arc<dyn Signaler>>>>,
}

impl SignalingRelay {
    pub fn new() -> Self {
        Self {
            websocket: Arc::new(Mutex::new(None)),
            overlay: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn set_delegate(&self, delegate: Arc<dyn Signaler>) {
        let mut lock = self.websocket.lock().unwrap_or_else(|e| e.into_inner());
        *lock = Some(delegate);
    }

    pub async fn set_overlay(&self, delegate: Arc<dyn Signaler>) {
        let mut lock = self.overlay.lock().unwrap_or_else(|e| e.into_inner());
        *lock = Some(delegate);
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Signaler for SignalingRelay {
    async fn send_signaling(
        &self,
        to: &NodeId,
        msg: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        let ws_delegate = {
            self.websocket
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
        let overlay_delegate = {
            self.overlay
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };

        if to.0 == "server" || to.0.is_empty() {
            if let Some(delegate) = ws_delegate {
                return delegate.send_signaling(to, msg).await;
            }
        } else {
            let mut sent = false;
            let mut last_err = None;

            if let Some(delegate) = ws_delegate {
                match delegate.send_signaling(to, msg.clone()).await {
                    Ok(_) => sent = true,
                    Err(e) => last_err = Some(e),
                }
            }
            if let Some(delegate) = overlay_delegate {
                match delegate.send_signaling(to, msg).await {
                    Ok(_) => sent = true,
                    Err(e) => last_err = Some(e),
                }
            }

            if sent {
                return Ok(());
            } else if let Some(e) = last_err {
                return Err(e);
            }
        }

        Err(mistlib_core::error::MistError::Internal(
            "SignalingRelay: no delegate set".to_string(),
        ))
    }
}
