use crate::error::Result;
use crate::types::{ConnectionState, DeliveryMethod, HostSendSync, NodeId};
use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;

pub mod bulk_pacing;
pub mod reliable_unordered;
pub mod send;
pub mod track_audience;

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
    /// Await local write completion. Implementations must not report mere
    /// enqueue success; unsupported transports explicitly return an error.
    async fn send_wait(&self, _node: &NodeId, _data: Bytes, _method: DeliveryMethod) -> Result<()> {
        Err(crate::error::MistError::Network(
            "write completion is unsupported".into(),
        ))
    }

    /// Provider sends normally await a real write. The overlay adapter uses
    /// legacy admission only when the storage pipeline is entirely disabled.
    async fn send_response(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> Result<()> {
        self.send_wait(node, data, method).await
    }

    /// Local lane hint for engine workers. Sequenced frames still use FIFO.
    async fn send_in_lane(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
        _lane: crate::action::SendLane,
        written: bool,
    ) -> Result<()> {
        if written {
            self.send_wait(node, data, method).await
        } else {
            self.send(node, data, method).await
        }
    }

    async fn broadcast(&self, data: Bytes, method: DeliveryMethod) -> Result<()>;
    fn get_connection_state(&self, node: &NodeId) -> ConnectionState;
    /// Whether data can currently reach this destination. Overlay transports
    /// include routed destinations; this does not change public session state.
    fn is_reachable(&self, node: &NodeId) -> bool {
        self.get_connection_state(node) == ConnectionState::Connected
    }
    async fn connect(&self, node: &NodeId) -> Result<()>;
    async fn disconnect(&self, node: &NodeId) -> Result<()>;

    /// Best-effort hint for the largest payload this transport can currently
    /// deliver to `node` in one `send`. `None` leaves callers to use their own
    /// conservative default; transports should surface the ceiling already
    /// enforced by their send path when it is known.
    fn message_size_limit(&self, _node: &NodeId) -> Option<u32> {
        None
    }

    /// Generation-owned admission budget shared with a paced transport.
    fn bulk_admission_budget(&self, _node: &NodeId) -> Option<Arc<send::ByteBudget>> {
        None
    }

    /// Local link identity for pinning an admitted send to a PC generation.
    /// Transports with their own generation-bound queue can retain None.
    fn send_generation(&self, _node: &NodeId) -> Option<u64> {
        None
    }

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
pub type ReliableUnordered = Channel<Reliable, Unordered>;
pub type UnreliableOrdered = Channel<Unreliable, Ordered>;
pub type UnreliableUnordered = Channel<Unreliable, Unordered>;
