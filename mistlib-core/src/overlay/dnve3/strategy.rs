use crate::action::OverlayAction;
use crate::config::Config;
use crate::overlay::dnve3::{DNVE3ConnectionBalancer, DNVE3DataStore, DNVE3Exchanger};
use crate::overlay::node_store::NodeStore;
use crate::overlay::routing_table::RoutingTable;
use crate::overlay::{ActionHandler, TopologyStrategy};
use crate::runtime::AsyncRuntime;
use crate::signaling::{
    OVERLAY_MSG_HEARTBEAT, OVERLAY_MSG_NODE_LIST, OVERLAY_MSG_PING, OVERLAY_MSG_PONG,
    OVERLAY_MSG_REQUEST_NODE_LIST,
};
use crate::types::NodeId;
use rand::Rng;
use std::sync::{Arc, Mutex};
use web_time::{Duration, Instant};

const BALANCER_JITTER_RATIO: f32 = 0.2;

#[derive(Clone, Copy)]
enum BalancePhase {
    FindImportant,
    SelectConnection,
}

pub struct DNVE3Strategy {
    pub balancer: DNVE3ConnectionBalancer,
    pub exchanger: DNVE3Exchanger,
    pub data_store: Arc<Mutex<DNVE3DataStore>>,
    pub node_store: Arc<Mutex<NodeStore>>,
    pub routing_table: Arc<Mutex<RoutingTable>>,
    pub local_node_id: NodeId,
    heartbeat_due_at: Mutex<Option<Instant>>,
    balancer_due_at: Mutex<Option<Instant>>,
    balancer_phase: Mutex<BalancePhase>,
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
            balancer: DNVE3ConnectionBalancer::new(data_store.clone(), routing_table.clone(), config),
            exchanger: DNVE3Exchanger::new(
                data_store.clone(),
                node_store.clone(),
                routing_table.clone(),
                local_node_id.clone(),
                config,
            ),
            data_store,
            node_store,
            routing_table,
            local_node_id,
            heartbeat_due_at: Mutex::new(None),
            balancer_due_at: Mutex::new(None),
            balancer_phase: Mutex::new(BalancePhase::FindImportant),
        }
    }

    fn heartbeat_interval(config: &Config) -> Duration {
        Duration::from_secs_f32(config.intervals.heartbeat.max(0.0))
    }

    fn balancer_interval_with_jitter(config: &Config) -> Duration {
        let base = config.intervals.connection_balancer.max(0.0);
        let mut rng = rand::thread_rng();
        let jitter = rng.gen_range(0.0..=(base * BALANCER_JITTER_RATIO));
        Duration::from_secs_f32(base + jitter)
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
        {
            let mut store = self.data_store.lock().unwrap();
            store.update_last_message_time(from);
        }
        if let Ok(mut rt) = self.routing_table.lock() {
            rt.add_message_node(from.clone());
        }

        match message_type {
            OVERLAY_MSG_HEARTBEAT => {
                self.exchanger.handle_heartbeat(from.clone(), payload);
                vec![]
            }
            OVERLAY_MSG_REQUEST_NODE_LIST => self.exchanger.handle_request_node_list(from.clone()),
            OVERLAY_MSG_NODE_LIST => {
                self.exchanger.handle_node_list(from.clone(), payload);
                vec![]
            }
            OVERLAY_MSG_PING => self.exchanger.handle_ping(from.clone(), payload),
            OVERLAY_MSG_PONG => {
                self.exchanger.handle_pong(from.clone(), payload);
                vec![]
            }
            _ => vec![],
        }
    }

    fn tick(
        &self,
        config: &Config,
        connected_node_states: &[(NodeId, crate::types::ConnectionState)],
    ) -> Vec<OverlayAction> {
        let mut actions = Vec::new();
        let now = Instant::now();
        let connected_nodes: Vec<_> = connected_node_states
            .iter()
            .filter(|(_, state)| *state == crate::types::ConnectionState::Connected)
            .map(|(id, _)| id.clone())
            .collect();

        {
            let mut due_lock = self.balancer_due_at.lock().unwrap();
            let mut phase_lock = self.balancer_phase.lock().unwrap();

            if due_lock.is_none() {
                *due_lock = Some(now + Self::balancer_interval_with_jitter(config));
            }

            if let Some(due_at) = *due_lock {
                if now >= due_at {
                    match *phase_lock {
                        BalancePhase::FindImportant => {
                            let has_density = {
                                let store = self.data_store.lock().unwrap();
                                store.self_density.is_some() && store.merged_density_map.is_some()
                            };

                            if has_density {
                                let important_nodes = self.balancer.find_important_nodes();
                                for (node_id, _) in important_nodes
                                    .into_iter()
                                    .take(config.limits.exchange_count as usize)
                                {
                                    actions.extend(self.exchanger.send_request_node_list(&node_id));
                                }
                            }
                            *phase_lock = BalancePhase::SelectConnection;
                        }
                        BalancePhase::SelectConnection => {
                            let (self_pos, all_nodes) = {
                                let store = self.node_store.lock().unwrap();
                                let self_pos = store
                                    .nodes
                                    .get(&self.local_node_id)
                                    .map(|n| n.position)
                                    .unwrap_or_else(crate::overlay::dnve3::Vector3::zero);
                                let all_nodes: Vec<_> = store
                                    .nodes
                                    .iter()
                                    .filter(|(id, _)| *id != &self.local_node_id)
                                    .map(|(id, n)| (id.clone(), n.position))
                                    .collect();
                                (self_pos, all_nodes)
                            };

                            actions.extend(self.balancer.select_connections(
                                config,
                                self_pos,
                                &all_nodes,
                                connected_node_states,
                                &self.local_node_id,
                            ));

                            *phase_lock = BalancePhase::FindImportant;
                        }
                    }

                    *due_lock = Some(now + Self::balancer_interval_with_jitter(config));
                }
            }
        }

        {
            let mut due_lock = self.heartbeat_due_at.lock().unwrap();

            if due_lock.is_none() {
                *due_lock = Some(now + Self::heartbeat_interval(config));
            }

            if let Some(due_at) = *due_lock {
                if now >= due_at {
                    self.exchanger.delete_old_data(config);
                    actions.extend(
                        self.exchanger
                            .update_and_send_heartbeat(config, &connected_nodes),
                    );
                    actions.extend(self.exchanger.send_ping_all(&connected_nodes));
                    *due_lock = Some(now + Self::heartbeat_interval(config));
                }
            }
        }

        actions
    }
}
