use crate::error::Result;
use crate::types::{DeliveryMethod, HostSendSync, NodeId};
use async_trait::async_trait;
use bytes::Bytes;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait L1Transport: HostSendSync {
    fn update_position(&self, x: f32, y: f32, z: f32);

    async fn send_message(
        &self,
        target_id: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> Result<()>;

    async fn broadcast(&self, data: Bytes, method: DeliveryMethod) -> Result<()>;
}

pub trait L1Notifier: HostSendSync {
    fn notify_connected(&self, node_id: &NodeId);
    fn notify_disconnected(&self, node_id: &NodeId);
    fn notify_node_position_updated(&self, node_id: &NodeId, x: f32, y: f32, z: f32);
}
