use async_trait::async_trait;
pub use mistlib_core::signaling::*;
use mistlib_core::types::SessionReestablishedHook;
use std::sync::Arc;
use tokio::sync::mpsc;

pub mod nostr;
pub mod ws;
pub use nostr::NostrSignaler;
pub use ws::WebSocketSignaler;

pub enum BootstrapSignaler {
    WebSocket(Arc<WebSocketSignaler>),
    Nostr(Arc<NostrSignaler>),
}

impl BootstrapSignaler {
    pub async fn connect(
        &self,
        incoming_tx: mpsc::Sender<MessageContent>,
    ) -> crate::error::Result<()> {
        match self {
            Self::WebSocket(signaler) => signaler.connect(incoming_tx).await,
            Self::Nostr(signaler) => signaler.connect(incoming_tx).await,
        }
    }
}

#[async_trait]
impl Signaler for BootstrapSignaler {
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
