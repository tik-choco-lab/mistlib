use crate::action::OverlayAction;
use crate::config::Config;
use crate::overlay::dnve3::data_store::DNVE3DataStore;
use crate::overlay::dnve3::exchanger::DNVE3Exchanger;
use crate::overlay::dnve3::spatial_density::Vector3;
use crate::overlay::dnve3::strategy::DNVE3Strategy;
use crate::overlay::dnve3::DNVE3ConnectionBalancer;
use crate::overlay::node_store::{NodeInfo, NodeStore};
use crate::overlay::routing_table::RoutingTable;
use crate::overlay::OverlayEnvelope;
use crate::signaling::MessageContent;
use crate::types::NodeId;
use std::sync::{Arc, Mutex};

pub fn test_config() -> Config {
    let mut c = Config::new_default();
    c.limits.max_connection_count = 5;
    c.limits.reserved_connection_count = 1;
    c.limits.force_disconnect_count = 1;
    c.limits.expire_node_seconds = 1.0;
    c.dnve.density_resolution = 6;
    c.dnve.distance_layers = 2;
    c.dnve.direction_threshold = 0.5;
    c.dnve.aoi_range = 100.0;
    c
}

pub fn node(s: &str) -> NodeId {
    NodeId(s.to_string())
}

pub fn pos(x: f32, y: f32, z: f32) -> Vector3 {
    Vector3::new(x, y, z)
}

pub fn make_balancer(config: &Config) -> DNVE3ConnectionBalancer {
    DNVE3ConnectionBalancer::new(config)
}

pub fn make_exchanger(
    local: &str,
) -> (
    DNVE3Exchanger,
    Arc<Mutex<DNVE3DataStore>>,
    Arc<Mutex<NodeStore>>,
) {
    let config = test_config();
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds.clone(), ns.clone(), rt, node(local), &config);
    (exchanger, ds, ns)
}

pub fn make_exchanger_full(
    id: &str,
    config: &Config,
) -> (
    DNVE3Exchanger,
    Arc<Mutex<DNVE3DataStore>>,
    Arc<Mutex<NodeStore>>,
    Arc<Mutex<RoutingTable>>,
) {
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let ex = DNVE3Exchanger::new(ds.clone(), ns.clone(), rt.clone(), node(id), config);
    (ex, ds, ns, rt)
}

pub fn make_strategy(local: &str) -> DNVE3Strategy {
    let config = test_config();
    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
    DNVE3Strategy::new(&config, node_store, node(local), routing_table)
}

pub fn extract_node_list_payload(action: &OverlayAction) -> Vec<u8> {
    match action {
        OverlayAction::SendMessage { data, .. } => {
            let env: OverlayEnvelope =
                bincode::deserialize(data).expect("SendMessage data should be a valid envelope");
            match env.content {
                MessageContent::Overlay(msg) => msg.payload,
                _ => vec![],
            }
        }
        _ => vec![],
    }
}

pub fn insert_pos(ns: &Arc<Mutex<NodeStore>>, id: &str, p: Vector3) {
    let node_id = node(id);
    ns.lock()
        .expect("NodeStore lock should not be poisoned")
        .nodes
        .insert(
            node_id.clone(),
            NodeInfo {
                id: node_id,
                position: p,
            },
        );
}
