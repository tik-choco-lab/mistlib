use async_trait::async_trait;
use mistlib_core::signaling::{MessageContent, Signaler};
use mistlib_core::types::NodeId;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct SignalingRelay {
    pub websocket: Arc<RwLock<Option<Arc<dyn Signaler>>>>,
    pub overlay: Arc<RwLock<Option<Arc<dyn Signaler>>>>,
}

impl SignalingRelay {
    pub fn new() -> Self {
        Self {
            websocket: Arc::new(RwLock::new(None)),
            overlay: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn set_websocket(&self, delegate: Arc<dyn Signaler>) {
        let mut lock = self.websocket.write().await;
        *lock = Some(delegate);
    }

    pub async fn set_overlay(&self, delegate: Arc<dyn Signaler>) {
        let mut lock = self.overlay.write().await;
        *lock = Some(delegate);
    }
}

#[async_trait]
impl Signaler for SignalingRelay {
    async fn send_signaling(
        &self,
        to: &NodeId,
        msg: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        let ws_delegate = self.websocket.read().await.clone();
        let overlay_delegate = self.overlay.read().await.clone();

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
