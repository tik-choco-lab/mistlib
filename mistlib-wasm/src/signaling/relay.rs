use async_trait::async_trait;
use mistlib_core::signaling::{MessageContent, Signaler};
use mistlib_core::types::ConnectionState;
use mistlib_core::types::NodeId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

pub struct SignalingRelay {
    pub websocket: Arc<Mutex<Option<Arc<dyn Signaler>>>>,
    pub overlay: Arc<Mutex<Option<Arc<dyn Signaler>>>>,
    pub connection_states: Arc<Mutex<Option<Arc<RwLock<HashMap<NodeId, ConnectionState>>>>>>,
}

impl SignalingRelay {
    pub fn new() -> Self {
        Self {
            websocket: Arc::new(Mutex::new(None)),
            overlay: Arc::new(Mutex::new(None)),
            connection_states: Arc::new(Mutex::new(None)),
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

    pub fn set_connection_states(
        &self,
        states: Arc<RwLock<HashMap<NodeId, ConnectionState>>>,
    ) {
        let mut lock = self
            .connection_states
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *lock = Some(states);
    }

    fn target_state(&self, target: &NodeId) -> Option<ConnectionState> {
        let states_opt = self
            .connection_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(states) = states_opt {
            let lock = states.read().unwrap_or_else(|e| e.into_inner());
            lock.get(target).cloned()
        } else {
            None
        }
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
            let target_connected = self.target_state(to) == Some(ConnectionState::Connected);

            if target_connected {
                if let Some(delegate) = overlay_delegate {
                    return delegate.send_signaling(to, msg).await;
                }
                if let Some(delegate) = ws_delegate {
                    return delegate.send_signaling(to, msg).await;
                }
            } else {
                if let Some(delegate) = ws_delegate {
                    return delegate.send_signaling(to, msg).await;
                }
                if let Some(delegate) = overlay_delegate {
                    return delegate.send_signaling(to, msg).await;
                }
            }
        }

        Err(mistlib_core::error::MistError::Internal(
            "SignalingRelay: no delegate set".to_string(),
        ))
    }
}
