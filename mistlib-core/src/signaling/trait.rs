use super::message::MessageContent;
use crate::error::Result;
use crate::types::{HostSendSync, NodeId, SessionReestablishedHook};
use async_trait::async_trait;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait Signaler: HostSendSync {
    async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> Result<()>;
    async fn reset_session(&self) -> Result<()> {
        Ok(())
    }

    /// Registers a hook called when the signaling session is reestablished.
    /// Implementations that do not support reconnect may leave this as a no-op.
    fn set_on_session_reestablished(&self, _hook: SessionReestablishedHook) {}

    async fn close(&self) -> Result<()>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait SignalingHandler: HostSendSync {
    async fn handle_message(&self, msg: MessageContent) -> Result<()>;
}
