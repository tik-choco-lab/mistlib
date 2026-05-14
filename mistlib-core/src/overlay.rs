pub mod dnve3;
pub mod message;
pub mod node_store;
pub mod router;
pub mod routing_table;
pub mod transport;

pub use message::{
    OverlayEnvelope, OverlayMessage, OVERLAY_MSG_HEARTBEAT, OVERLAY_MSG_NODE_LIST,
    OVERLAY_MSG_PING, OVERLAY_MSG_PONG, OVERLAY_MSG_POSITION, OVERLAY_MSG_REQUEST_NODE_LIST,
};
pub use node_store::{NodeInfo, NodeStore};
pub use router::OverlayRouter;
pub use transport::OverlayTransport;

use crate::action::OverlayAction;
use crate::config::Config;
use crate::runtime::AsyncRuntime;
use crate::types::NodeId;
use async_trait::async_trait;
use std::sync::Arc;

#[cfg(target_arch = "wasm32")]
pub trait ActionHandler {
    fn handle_action(&self, action: OverlayAction);
}

#[cfg(not(target_arch = "wasm32"))]
pub trait ActionHandler: Send + Sync {
    fn handle_action(&self, action: OverlayAction);
}

#[cfg(target_arch = "wasm32")]
#[async_trait(?Send)]
pub trait TopologyStrategy {
    async fn start(
        &self,
        runtime: Arc<dyn AsyncRuntime>,
        config: Arc<Config>,
        action_handler: Arc<dyn ActionHandler>,
    );

    fn handle_message(
        &self,
        from: &NodeId,
        message_type: u32,
        payload: &[u8],
    ) -> Vec<OverlayAction>;

    fn tick(
        &self,
        config: &Config,
        connected_node_states: &[(NodeId, crate::types::ConnectionState)],
    ) -> Vec<OverlayAction>;
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
pub trait TopologyStrategy: Send + Sync {
    async fn start(
        &self,
        runtime: Arc<dyn AsyncRuntime>,
        config: Arc<Config>,
        action_handler: Arc<dyn ActionHandler>,
    );

    fn handle_message(
        &self,
        from: &NodeId,
        message_type: u32,
        payload: &[u8],
    ) -> Vec<OverlayAction>;

    fn tick(
        &self,
        config: &Config,
        connected_node_states: &[(NodeId, crate::types::ConnectionState)],
    ) -> Vec<OverlayAction>;
}
