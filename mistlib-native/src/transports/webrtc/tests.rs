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

    async fn close(&self) -> MistResult<()> {
        Ok(())
    }
}

pub fn make_transport() -> WebRtcTransport {
    WebRtcTransport::new(Arc::new(MockSignaler), NodeId("local".to_string()))
}

pub mod basic;
pub mod cleanup;
pub mod conn_timing;
pub mod disconnect;
pub mod ice_restart;
pub mod ice_servers;
pub mod limits;
pub mod message_size;
pub mod pending_candidates;
pub mod publish;
pub mod reorder;
pub mod send_queues;
pub mod signaling;
pub mod sweeper;
pub mod takeover;
