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
    pub(super) fn preferred_transport(&self) -> &dyn Transport {
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
}

pub trait EngineEventHandler: Send + Sync {
    fn on_event(&self, event: EngineEvent);
}
