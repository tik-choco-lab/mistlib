use super::message::MessageContent;
use crate::error::Result;
use crate::types::{HostSendSync, NodeId};
use async_trait::async_trait;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait Signaler: HostSendSync {
    async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> Result<()>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait SignalingHandler: HostSendSync {
    async fn handle_message(&self, msg: MessageContent) -> Result<()>;
}
