//! Separate process so the process-wide SCTP default cannot race other tests.
use async_trait::async_trait;
use mistlib::transports::webrtc::WebRtcTransport;
use mistlib_core::{
    config::StorageTransferConfig,
    signaling::{MessageContent, Signaler},
    types::NodeId,
};
use std::sync::Arc;
struct NoSignaler;
#[async_trait]
impl Signaler for NoSignaler {
    async fn send_signaling(
        &self,
        _: &NodeId,
        _: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        Ok(())
    }
    async fn close(&self) -> mistlib_core::error::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn native_flag_sets_new_association_default_and_zero_restores_upstream_value() {
    assert_eq!(
        sctp::association::default_receive_buffer_size(),
        1024 * 1024
    );
    let transport = WebRtcTransport::new(Arc::new(NoSignaler), NodeId("native-buffer-test".into()));
    transport.set_storage_transfer(StorageTransferConfig {
        native_sctp_receive_buffer_bytes: 8 * 1024 * 1024,
        ..Default::default()
    });
    assert_eq!(
        sctp::association::default_receive_buffer_size(),
        8 * 1024 * 1024
    );
    transport.set_storage_transfer(StorageTransferConfig::default());
    assert_eq!(
        sctp::association::default_receive_buffer_size(),
        1024 * 1024
    );
}
