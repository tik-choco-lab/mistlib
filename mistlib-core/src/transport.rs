use crate::error::Result;
use crate::types::{ConnectionState, DeliveryMethod, HostSendSync, NodeId};
use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct NetworkEvent {
    pub from: NodeId,
    pub data: Bytes,
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait NetworkEventHandler: HostSendSync {
    fn on_event(&self, event: NetworkEvent);
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait Transport: HostSendSync {
    async fn start(&self, handler: Arc<dyn NetworkEventHandler>) -> Result<()>;
    async fn send(&self, node: &NodeId, data: Bytes, method: DeliveryMethod) -> Result<()>;
    async fn broadcast(&self, data: Bytes, method: DeliveryMethod) -> Result<()>;
    fn get_connection_state(&self, node: &NodeId) -> ConnectionState;
    async fn connect(&self, node: &NodeId) -> Result<()>;
    async fn disconnect(&self, node: &NodeId) -> Result<()>;

    /// Called when overlay-level PING/PONG liveness suspects `node` may be
    /// disconnected. Transports that track a reconnect-grace period (e.g. for
    /// ICE Disconnected) should fold this into the same flow, recording that
    /// this particular grace period was liveness-suspect-originated so
    /// `clear_suspect` knows whether it may cancel it. No-op by default.
    async fn suspect_disconnected(&self, _node: &NodeId) -> Result<()> {
        Ok(())
    }

    /// Cancels a liveness-suspect grace period previously started by
    /// `suspect_disconnected` for `node` (e.g. because a PONG arrived after
    /// all). Must be a no-op if `node` has no active grace period, or if its
    /// grace period was started by something other than
    /// `suspect_disconnected` (e.g. ICE Disconnected) -- that kind only ends
    /// via the transport's own recovery signal. No-op by default.
    async fn clear_suspect(&self, _node: &NodeId) -> Result<()> {
        Ok(())
    }

    fn get_connected_nodes(&self) -> Vec<NodeId>;
    fn get_active_connection_states(&self) -> Vec<(NodeId, ConnectionState)> {
        self.get_connected_nodes()
            .into_iter()
            .map(|id| (id, ConnectionState::Connected))
            .collect()
    }
}

pub trait Reliability: Send + Sync {}
pub struct Reliable;
pub struct Unreliable;
impl Reliability for Reliable {}
impl Reliability for Unreliable {}

pub trait Ordering: Send + Sync {}
pub struct Ordered;
pub struct Unordered;
impl Ordering for Ordered {}
impl Ordering for Unordered {}

pub struct Channel<R: Reliability, O: Ordering> {
    pub inner: Arc<dyn Transport>,
    pub method: DeliveryMethod,
    _phantom: std::marker::PhantomData<(R, O)>,
}

impl<R: Reliability, O: Ordering> Channel<R, O> {
    pub fn new(transport: Arc<dyn Transport>, method: DeliveryMethod) -> Self {
        Self {
            inner: transport,
            method,
            _phantom: std::marker::PhantomData,
        }
    }

    pub async fn send(&self, node: &NodeId, data: Bytes) -> Result<()> {
        self.inner.send(node, data, self.method).await
    }

    pub async fn broadcast(&self, data: Bytes) -> Result<()> {
        self.inner.broadcast(data, self.method).await
    }
}

pub type ReliableOrdered = Channel<Reliable, Ordered>;
pub type UnreliableOrdered = Channel<Unreliable, Ordered>;
pub type UnreliableUnordered = Channel<Unreliable, Unordered>;
