use crate::overlay::OverlayRouter;
use crate::signaling::{Signaler, SignalingHandler};
use crate::transport::Transport;
use crate::types::{ConnectionState, NodeId};
use bytes::Bytes;
use std::sync::Arc;

#[derive(Clone)]
pub struct RunningContext {
    pub transport: Arc<dyn Transport>,
    pub network_transport: Option<Arc<dyn Transport>>,
    pub signaling_handler: Arc<dyn SignalingHandler>,
    pub p2p_signaling_handler: Option<Arc<dyn SignalingHandler>>,
    pub signaling_dispatch: Option<Arc<dyn Signaler>>,
    pub websocket_signaler: Option<Arc<dyn Signaler>>,
    pub overlay: Option<Arc<OverlayRouter>>,
}

impl RunningContext {
    /// Returns the network transport when present, otherwise the primary transport.
    ///
    /// Use this to dispatch data that has already been serialized into an
    /// `OverlayEnvelope` (e.g. via `overlay.wrap_data`). Sending such data
    /// through `self.transport` (`OverlayTransport`) instead would wrap it a
    /// second time, since `OverlayTransport::send`/`broadcast` call
    /// `router.dispatch_data` internally.
    pub fn preferred_transport(&self) -> &dyn Transport {
        self.network_transport
            .as_deref()
            .unwrap_or(&*self.transport)
    }

    /// Returns the active connection states from the appropriate transport.
    pub(super) fn active_connection_states(&self) -> Vec<(NodeId, ConnectionState)> {
        if let Some(nt) = &self.network_transport {
            nt.get_active_connection_states()
        } else {
            self.transport.get_active_connection_states()
        }
    }
}

pub enum EngineState {
    Idle,
    Running(Arc<RunningContext>),
}

pub enum EngineEvent {
    RawMessage(NodeId, Bytes),
    OverlayMessage(NodeId, Vec<u8>),
    NeighborsUpdated(Vec<u8>),
    AoiEntered(NodeId),
    AoiLeft(NodeId),
    AoiNodesUpdated(Vec<u8>),
    NodePositionUpdated(NodeId, Vec<u8>),
    AllConnectionsLost(NodeId),
}

impl EngineEvent {
    /// Common callback code, sender and bytes. The adapter supplies its room id.
    pub fn into_application_event(self) -> (u32, NodeId, Vec<u8>) {
        use crate::events::*;
        let rust = || NodeId("rust".to_string());
        match self {
            Self::RawMessage(id, data) => (EVENT_RAW, id, data.to_vec()),
            Self::OverlayMessage(id, data) => (EVENT_OVERLAY, id, data),
            Self::NeighborsUpdated(data) => (EVENT_NEIGHBORS, rust(), data),
            Self::AoiEntered(id) => (EVENT_AOI_ENTERED, id, b"entered".to_vec()),
            Self::AoiLeft(id) => (EVENT_AOI_LEFT, id, b"left".to_vec()),
            Self::AoiNodesUpdated(data) => (EVENT_AOI_NODES, rust(), data),
            Self::NodePositionUpdated(id, data) => (EVENT_NODE_POSITION_UPDATED, id, data),
            Self::AllConnectionsLost(id) => (
                EVENT_ALL_CONNECTIONS_LOST,
                id,
                ALL_CONNECTIONS_LOST_PAYLOAD.to_vec(),
            ),
        }
    }
}

pub trait EngineEventHandler: Send + Sync {
    fn on_event(&self, event: EngineEvent);
}
