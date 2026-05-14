mod guidance;
mod messages;
mod selection;
mod tick;

use crate::action::OverlayAction;
use crate::config::{Config, ConnectionMode};
use crate::overlay::dnve3::{DNVE3ConnectionBalancer, DNVE3DataStore, DNVE3Exchanger};
use crate::overlay::node_store::NodeStore;
use crate::overlay::routing_table::RoutingTable;
use crate::overlay::{ActionHandler, TopologyStrategy};
use crate::runtime::AsyncRuntime;
use crate::stats::ping;
use crate::types::{ConnectionState, NodeId};
use std::sync::{Arc, Mutex};
use web_time::Instant;

pub struct DNVE3Strategy {
    balancer: DNVE3ConnectionBalancer,
    exchanger: DNVE3Exchanger,
    data_store: Arc<Mutex<DNVE3DataStore>>,
    pub(crate) node_store: Arc<Mutex<NodeStore>>,
    local_node_id: NodeId,
    hop_count: u32,
    heartbeat_due_at: Mutex<Option<Instant>>,
    balancer_due_at: Mutex<Option<Instant>>,
    node_list_due_at: Mutex<Option<Instant>>,
    /// handle_message は config を受け取らないため、tick() で最新値に更新する
    connection_mode: Mutex<ConnectionMode>,
}

impl DNVE3Strategy {
    pub fn new(
        config: &Config,
        node_store: Arc<Mutex<NodeStore>>,
        local_node_id: NodeId,
        routing_table: Arc<Mutex<RoutingTable>>,
    ) -> Self {
        let data_store = Arc::new(Mutex::new(DNVE3DataStore::new()));
        Self {
            balancer: DNVE3ConnectionBalancer::new(config),
            exchanger: DNVE3Exchanger::new(
                data_store.clone(),
                node_store.clone(),
                routing_table.clone(),
                local_node_id.clone(),
                config,
            ),
            data_store,
            node_store,
            local_node_id,
            hop_count: config.limits.hop_count,
            heartbeat_due_at: Mutex::new(None),
            balancer_due_at: Mutex::new(None),
            node_list_due_at: Mutex::new(None),
            connection_mode: Mutex::new(config.dnve.connection_mode),
        }
    }

    pub(super) fn remember_connection_mode(&self, mode: ConnectionMode) {
        *self
            .connection_mode
            .lock()
            .expect("connection_mode lock poisoned") = mode;
    }

    pub(super) fn current_connection_mode(&self) -> ConnectionMode {
        *self
            .connection_mode
            .lock()
            .expect("connection_mode lock poisoned")
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl TopologyStrategy for DNVE3Strategy {
    async fn start(
        &self,
        _runtime: Arc<dyn AsyncRuntime>,
        _config: Arc<Config>,
        _action_handler: Arc<dyn ActionHandler>,
    ) {
    }

    fn handle_message(
        &self,
        from: &NodeId,
        message_type: u32,
        payload: &[u8],
    ) -> Vec<OverlayAction> {
        self.mark_peer_seen(from);

        match message_type {
            crate::overlay::OVERLAY_MSG_HEARTBEAT => self.handle_heartbeat_message(from, payload),
            crate::overlay::OVERLAY_MSG_REQUEST_NODE_LIST => {
                self.handle_request_node_list_message(from, payload)
            }
            crate::overlay::OVERLAY_MSG_NODE_LIST => self.handle_node_list_message(from, payload),
            crate::overlay::OVERLAY_MSG_POSITION => self.handle_position_message(from, payload),
            crate::overlay::OVERLAY_MSG_PING => {
                ping::handle_ping(&self.local_node_id, from.clone(), self.hop_count, payload)
            }
            crate::overlay::OVERLAY_MSG_PONG => {
                ping::handle_pong(from.clone(), payload);
                vec![]
            }
            _ => vec![],
        }
    }

    fn tick(
        &self,
        config: &Config,
        connected_node_states: &[(NodeId, ConnectionState)],
    ) -> Vec<OverlayAction> {
        let mode = config.dnve.connection_mode;
        self.remember_connection_mode(mode);

        let now = Instant::now();
        let connected_nodes = Self::connected_node_ids(connected_node_states);
        let density_guidance = self.maybe_density_guidance(config, mode);

        self.collect_tick_actions(
            config,
            now,
            connected_node_states,
            &connected_nodes,
            mode,
            density_guidance.as_ref(),
        )
    }
}
