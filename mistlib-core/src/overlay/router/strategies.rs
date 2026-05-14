use super::OverlayRouter;
use crate::action::OverlayAction;
use crate::config::Config;
use crate::overlay::TopologyStrategy;
use crate::types::{ConnectionState, NodeId};
use std::sync::Arc;

impl OverlayRouter {
    pub fn add_strategy(&mut self, strategy: Arc<dyn TopologyStrategy>) {
        self.strategies.push(strategy);
    }

    pub async fn start(
        &self,
        runtime: Arc<dyn crate::runtime::AsyncRuntime>,
        config: Arc<Config>,
        action_handler: Arc<dyn crate::overlay::ActionHandler>,
    ) {
        for strategy in &self.strategies {
            strategy
                .start(runtime.clone(), config.clone(), action_handler.clone())
                .await;
        }
    }

    pub fn handle_overlay_message(
        &self,
        from: NodeId,
        message_type: u32,
        payload: &[u8],
    ) -> Vec<OverlayAction> {
        self.strategies
            .iter()
            .flat_map(|s| s.handle_message(&from, message_type, payload))
            .collect()
    }

    pub fn tick(
        &self,
        config: &Config,
        connected_node_states: &[(NodeId, ConnectionState)],
    ) -> Vec<OverlayAction> {
        self.strategies
            .iter()
            .flat_map(|s| s.tick(config, connected_node_states))
            .collect()
    }
}
