use async_trait::async_trait;
pub use mistlib_core::signaling::*;
use mistlib_core::types::SessionReestablishedHook;
use std::sync::Arc;
use tokio::sync::mpsc;
use wasm_bindgen::JsValue;

pub mod nostr;
pub mod ws;

pub use nostr::WasmNostrSignaler;
pub use ws::WasmWebSocketSignaler;

pub enum WasmBootstrapSignaler {
    WebSocket(Arc<WasmWebSocketSignaler>),
    Nostr(Arc<WasmNostrSignaler>),
}

impl WasmBootstrapSignaler {
    pub async fn connect(&self, tx: mpsc::UnboundedSender<MessageContent>) -> Result<(), JsValue> {
        match self {
            Self::WebSocket(signaler) => signaler.connect(tx).await,
            Self::Nostr(signaler) => signaler.connect(tx).await,
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Signaler for WasmBootstrapSignaler {
    async fn send_signaling(
        &self,
        to: &mistlib_core::types::NodeId,
        msg: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        match self {
            Self::WebSocket(signaler) => signaler.send_signaling(to, msg).await,
            Self::Nostr(signaler) => signaler.send_signaling(to, msg).await,
        }
    }

    async fn note_peer_alive(&self, peer: &mistlib_core::types::NodeId) {
        match self {
            Self::WebSocket(signaler) => signaler.note_peer_alive(peer).await,
            Self::Nostr(signaler) => signaler.note_peer_alive(peer).await,
        }
    }

    async fn reset_session(&self) -> mistlib_core::error::Result<()> {
        match self {
            Self::WebSocket(signaler) => signaler.reset_session().await,
            Self::Nostr(signaler) => signaler.reset_session().await,
        }
    }

    fn set_on_session_reestablished(&self, hook: SessionReestablishedHook) {
        match self {
            Self::WebSocket(signaler) => signaler.set_on_session_reestablished(hook),
            Self::Nostr(signaler) => signaler.set_on_session_reestablished(hook),
        }
    }

    async fn close(&self) -> mistlib_core::error::Result<()> {
        match self {
            Self::WebSocket(signaler) => signaler.close().await,
            Self::Nostr(signaler) => signaler.close().await,
        }
    }
}
