use super::*;
use async_trait::async_trait;
use mistlib_core::{
    error::Result as MistResult,
    signaling::{MessageContent, Signaler},
    types::NodeId,
};
use std::sync::Arc;

pub struct MockSignaler;

#[async_trait]
impl Signaler for MockSignaler {
    async fn send_signaling(&self, _to: &NodeId, _msg: MessageContent) -> MistResult<()> {
        Ok(())
    }
}

pub fn make_transport() -> WebRtcTransport {
    WebRtcTransport::new(Arc::new(MockSignaler), NodeId("local".to_string()))
}

pub mod basic;
pub mod limits;
pub mod signaling;
