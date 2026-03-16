#![cfg(test)]

use crate::config::Config;
use crate::config::DensityEncoding;
use crate::overlay::dnve3::data_store::DNVE3DataStore;
use crate::overlay::dnve3::exchanger::{DNVE3Exchanger, HeartbeatDensityPayload};
use crate::overlay::dnve3::spatial_density::{SpatialDensityUtils, Vector3};
use crate::overlay::dnve3::strategy::DNVE3Strategy;
use crate::overlay::dnve3::DNVE3ConnectionBalancer;
use crate::overlay::node_store::{NodeInfo, NodeStore};
use crate::overlay::routing_table::RoutingTable;
use crate::overlay::TopologyStrategy;
use crate::signaling::{MessageContent, SignalingEnvelope};
use crate::signaling::{OVERLAY_MSG_HEARTBEAT, OVERLAY_MSG_PING, OVERLAY_MSG_PONG};
use crate::types::{ConnectionState, NodeId};
use std::sync::{Arc, Mutex};

fn test_config() -> Config {
    let mut c = Config::new_default();
    c.limits.max_connection_count = 5;
    c.limits.reserved_connection_count = 1;
    c.limits.force_disconnect_count = 1;
    c.limits.expire_node_seconds = 1;
    c.limits.exchange_count = 3;
    c.dnve.density_resolution = 6;
    c.dnve.distance_layers = 2;
    c.dnve.direction_threshold = 0.5;
    c.dnve.aoi_range = 100.0;
    c
}

fn node(s: &str) -> NodeId {
    NodeId(s.to_string())
}

fn pos(x: f32, y: f32, z: f32) -> Vector3 {
    Vector3::new(x, y, z)
}

fn make_strategy(local: &str) -> DNVE3Strategy {
    let config = test_config();
    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new(60)));
    DNVE3Strategy::new(&config, node_store, node(local), routing_table)
}

#[test]
fn spatial_density_empty_nodes_all_zeros() {
    let utils = SpatialDensityUtils::new(6);
    let data = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[], 2, 0.0);
    assert!(
        data.density_map.iter().all(|&v| v == 0.0),
        "ノードが空なら density_map は全て 0 であるべき"
    );
}

#[test]
fn spatial_density_single_node_has_nonzero_entry() {
    let utils = SpatialDensityUtils::new(6);
    let data = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[pos(10.0, 0.0, 0.0)], 2, 0.0);
    let has_nonzero = data.density_map.iter().any(|&v| v > 0.0);
    assert!(
        has_nonzero,
        "1 つのノードがあれば density_map に非ゼロ値があるべき"
    );
}

#[test]
fn spatial_density_dir_count_matches_resolution() {
    let resolution = 8;
    let utils = SpatialDensityUtils::new(resolution);
    let data = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[], 3, 0.0);
    assert_eq!(data.dir_count, resolution);
    assert_eq!(data.layer_count, 3);
    assert_eq!(data.density_map.len(), resolution * 3);
}

#[test]
fn spatial_density_byte_roundtrip_keeps_shape_and_approximates_values() {
    let utils = SpatialDensityUtils::new(12);
    let original = utils.create_spatial_density(
        pos(0.0, 0.0, 0.0),
        &[
            pos(10.0, 0.0, 0.0),
            pos(-4.0, 7.0, 2.0),
            pos(3.0, -6.0, 1.0),
        ],
        3,
        0.0,
    );

    let encoded = original.to_byte_encoded();
    let decoded =
        crate::overlay::dnve3::spatial_density::SpatialDensityData::from_byte_encoded(&encoded);

    assert_eq!(decoded.dir_count, original.dir_count);
    assert_eq!(decoded.layer_count, original.layer_count);
    assert_eq!(decoded.density_map.len(), original.density_map.len());

    let max_value = original
        .density_map
        .iter()
        .copied()
        .fold(0.0_f32, |acc, v| acc.max(v));
    let tolerance = if max_value <= 0.0 {
        1e-6
    } else {
        (max_value / 255.0) + 1e-6
    };
    for (o, d) in original.density_map.iter().zip(decoded.density_map.iter()) {
        assert!(
            (o - d).abs() <= tolerance,
            "byte roundtrip error too large: original={o}, decoded={d}, tolerance={tolerance}"
        );
    }
}

#[test]
fn project_spatial_density_same_center_returns_clone() {
    let utils = SpatialDensityUtils::new(6);
    let data = utils.create_spatial_density(pos(5.0, 0.0, 0.0), &[pos(10.0, 0.0, 0.0)], 2, 0.0);
    let projected = utils.project_spatial_density(&data, pos(5.0, 0.0, 0.0));
    assert_eq!(
        data.density_map, projected.density_map,
        "同じ中心への projection は完全に同じマップを返すべき"
    );
}

#[test]
fn project_spatial_density_different_center_does_not_panic() {
    let utils = SpatialDensityUtils::new(6);
    let data = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[pos(10.0, 0.0, 0.0)], 2, 0.0);

    let _projected = utils.project_spatial_density(&data, pos(20.0, 0.0, 0.0));
}

#[test]
fn merge_spatial_density_increases_values() {
    let utils = SpatialDensityUtils::new(6);
    let a = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[pos(5.0, 0.0, 0.0)], 2, 0.0);
    let b = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[pos(-5.0, 0.0, 0.0)], 2, 0.0);
    let merged = utils.merge_spatial_density(&a, &b);

    let sum_a: f32 = a.density_map.iter().sum();
    let sum_b: f32 = b.density_map.iter().sum();
    let sum_m: f32 = merged.density_map.iter().sum();
    assert!(
        sum_m >= sum_a,
        "マージ後の合計は A の合計以上であるべき(sum_m={sum_m}, sum_a={sum_a}, sum_b={sum_b})"
    );
}

fn make_balancer(config: &Config) -> DNVE3ConnectionBalancer {
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new(60)));
    DNVE3ConnectionBalancer::new(ds, rt, config)
}

#[test]
fn balancer_find_important_no_self_density_returns_empty() {
    let config = test_config();
    let balancer = make_balancer(&config);
    let result = balancer.find_important_nodes();
    assert!(
        result.is_empty(),
        "self_density が未設定なら find_important_nodes は空を返すべき"
    );
}

#[test]
fn balancer_select_connections_empty_world() {
    let config = test_config();
    let balancer = make_balancer(&config);
    let actions = balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &[], &[], &node("self"));
    assert!(actions.is_empty(), "ノードが存在しない場合はアクションなし");
}

#[test]
fn balancer_select_connections_connects_nearby_node() {
    let mut config = test_config();
    config.dnve.density_resolution = 1;
    let balancer = make_balancer(&config);
    let self_id = node("self");
    let peer = node("peer-a");

    let all_nodes = vec![(peer.clone(), pos(0.0, 0.0, 10.0))];
    let actions =
        balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    let has_connect = actions
        .iter()
        .any(|a| matches!(a, crate::action::OverlayAction::Connect { to } if *to == peer));
    assert!(
        has_connect,
        "近くのノードへの Connect アクションが生成されるべき"
    );
}

#[test]
fn balancer_select_connections_skips_self() {
    let config = test_config();
    let balancer = make_balancer(&config);
    let self_id = node("self");

    let all_nodes = vec![(self_id.clone(), pos(0.0, 0.0, 0.0))];
    let actions =
        balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    let connects_to_self = actions
        .iter()
        .any(|a| matches!(a, crate::action::OverlayAction::Connect { to } if *to == self_id));
    assert!(!connects_to_self, "自分自身への Connect は生成されないはず");
}

#[test]
fn balancer_disconnects_when_over_target() {
    let mut config = test_config();
    config.limits.max_connection_count = 3;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");

    let connected: Vec<(NodeId, ConnectionState)> = (0..4)
        .map(|i| (node(&format!("peer-{i}")), ConnectionState::Connected))
        .collect();

    let all_nodes: Vec<(NodeId, Vector3)> = connected
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.clone(), pos(i as f32 * 10.0, 0.0, 0.0)))
        .collect();

    let actions = balancer.select_connections(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &connected,
        &self_id,
    );

    let disconnect_count = actions
        .iter()
        .filter(|a| matches!(a, crate::action::OverlayAction::Disconnect { .. }))
        .count();

    assert!(
        disconnect_count >= 1,
        "max を超えた場合は少なくとも 1 つの Disconnect が生成されるべき(got {disconnect_count})"
    );
}

#[test]
fn balancer_does_not_reconnect_already_connected_node() {
    let config = test_config();
    let balancer = make_balancer(&config);
    let self_id = node("self");
    let peer = node("peer-a");

    let all_nodes = vec![(peer.clone(), pos(10.0, 0.0, 0.0))];
    let connected = vec![(peer.clone(), ConnectionState::Connected)];

    let actions = balancer.select_connections(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &connected,
        &self_id,
    );

    let connect_to_peer = actions
        .iter()
        .filter(|a| matches!(a, crate::action::OverlayAction::Connect { to } if *to == peer))
        .count();

    assert_eq!(
        connect_to_peer, 0,
        "既に Connected のノードへ Connect アクションは生成されないはず"
    );
}

fn make_exchanger(
    local: &str,
) -> (
    DNVE3Exchanger,
    Arc<Mutex<DNVE3DataStore>>,
    Arc<Mutex<NodeStore>>,
) {
    let config = test_config();
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new(60)));
    let exchanger = DNVE3Exchanger::new(ds.clone(), ns.clone(), rt, node(local), &config);
    (exchanger, ds, ns)
}

#[test]
fn exchanger_handle_heartbeat_updates_data_store() {
    let (exchanger, ds, _) = make_exchanger("local");
    let config = test_config();

    let utils = SpatialDensityUtils::new(config.dnve.density_resolution as usize);
    let density = utils.create_spatial_density(
        pos(5.0, 0.0, 0.0),
        &[pos(10.0, 0.0, 0.0)],
        config.dnve.distance_layers as usize,
        0.0,
    );
    let payload = bincode::serialize(&density).unwrap();

    exchanger.handle_heartbeat(node("peer-a"), &payload);

    let store = ds.lock().unwrap();
    assert!(
        store.neighbors.contains_key(&node("peer-a")),
        "handle_heartbeat 後の data_store に neighbor が登録されるべき"
    );
}

#[test]
fn exchanger_heartbeat_byte_payload_roundtrip_is_accepted() {
    let mut config = test_config();
    config.dnve.density_encoding = DensityEncoding::Byte;

    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new(60)));
    let exchanger = DNVE3Exchanger::new(ds.clone(), ns.clone(), rt, node("local"), &config);

    {
        let mut store = ns.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
    }

    let actions = exchanger.update_and_send_heartbeat(&config, &[node("peer-a")]);
    assert_eq!(actions.len(), 1, "heartbeat action should be produced");

    let payload = match &actions[0] {
        crate::action::OverlayAction::SendMessage { data, .. } => {
            let env: SignalingEnvelope = bincode::deserialize(data).unwrap();
            match env.content {
                MessageContent::Overlay(msg) => msg.payload,
                _ => vec![],
            }
        }
        _ => vec![],
    };
    assert!(!payload.is_empty(), "heartbeat payload should not be empty");

    let decoded: HeartbeatDensityPayload =
        bincode::deserialize(&payload).expect("byte heartbeat payload should decode");
    assert!(
        matches!(decoded, HeartbeatDensityPayload::Byte(_)),
        "byte config should emit Byte heartbeat payload"
    );

    exchanger.handle_heartbeat(node("peer-a"), &payload);
    let store = ds.lock().unwrap();
    assert!(
        store.neighbors.contains_key(&node("peer-a")),
        "byte heartbeat payload should be accepted by receiver"
    );
}

#[test]
fn exchanger_handle_heartbeat_bad_payload_is_noop() {
    let (exchanger, ds, _) = make_exchanger("local");

    exchanger.handle_heartbeat(node("peer-x"), b"garbage-data");
    let store = ds.lock().unwrap();
    assert!(
        store.neighbors.is_empty(),
        "不正な payload は無視されるべき (パニックしてはいけない)"
    );
}

#[test]
fn exchanger_delete_old_data_removes_expired_neighbors() {
    let (exchanger, ds, ns) = make_exchanger("local");
    let config = test_config();

    {
        let utils = SpatialDensityUtils::new(config.dnve.density_resolution as usize);
        let density = utils.create_spatial_density(pos(0.0, 0.0, 0.0), &[], 2, 0.0);
        let mut store = ds.lock().unwrap();
        store.add_or_update_neighbor(node("stale"), density.clone());
        store.add_or_update_neighbor(node("fresh"), density);
    }

    let mut fast_config = config.clone();
    fast_config.limits.expire_node_seconds = 0;

    {
        let mut node_store = ns.lock().unwrap();
        node_store.nodes.insert(
            node("stale"),
            NodeInfo {
                id: node("stale"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
    }

    exchanger.delete_old_data(&fast_config);

    let store = ds.lock().unwrap();

    assert!(
        store.neighbors.is_empty(),
        "expire_node_seconds=0 ならすべての neighbor が削除されるべき"
    );
}

#[test]
fn exchanger_send_request_node_list_returns_one_action() {
    let (exchanger, _, _) = make_exchanger("local");
    let actions = exchanger.send_request_node_list(&node("target"));
    assert_eq!(
        actions.len(),
        1,
        "send_request_node_list は 1 つのアクションを返すべき"
    );
}

#[test]
fn exchanger_handle_ping_returns_pong_action() {
    let (exchanger, _, _) = make_exchanger("local");
    let payload: Vec<u8> = 12345u64.to_le_bytes().to_vec();
    let actions = exchanger.handle_ping(node("sender"), &payload);
    assert_eq!(
        actions.len(),
        1,
        "handle_ping は pong action を 1 つ返すべき"
    );

    if let crate::action::OverlayAction::SendMessage { data, .. } = &actions[0] {
        use crate::signaling::SignalingEnvelope;
        let envelope: SignalingEnvelope = bincode::deserialize(data).unwrap();
        if let crate::signaling::MessageContent::Overlay(msg) = &envelope.content {
            assert_eq!(msg.message_type, OVERLAY_MSG_PONG);
            assert_eq!(
                &msg.payload, &payload,
                "pong payload は ping と同じであるべき"
            );
        } else {
            panic!("pong メッセージは Overlay type であるべき");
        }
    }
}

#[test]
fn exchanger_handle_pong_short_payload_is_noop() {
    let (exchanger, _, _) = make_exchanger("local");

    exchanger.handle_pong(node("peer"), b"short");
}

#[test]
fn exchanger_update_and_send_heartbeat_returns_actions_per_connected_node() {
    let config = test_config();
    let (exchanger, _, ns) = make_exchanger("local");

    {
        let mut store = ns.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
    }

    let connected = vec![node("peer-a"), node("peer-b")];
    let actions = exchanger.update_and_send_heartbeat(&config, &connected);
    assert_eq!(
        actions.len(),
        connected.len(),
        "update_and_send_heartbeat は接続ノード数ぶんの送信アクションを返すべき"
    );
    let sent_to: std::collections::HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            crate::action::OverlayAction::SendMessage { to, .. } => Some(to.clone()),
            _ => None,
        })
        .collect();
    assert!(
        sent_to.contains(&node("peer-a")) && sent_to.contains(&node("peer-b")),
        "heartbeat は各 connected peer に個別送信されるべき"
    );
}

#[test]
fn exchanger_update_and_send_heartbeat_no_connected_nodes_returns_empty() {
    let config = test_config();
    let (exchanger, _, ns) = make_exchanger("local");

    {
        let mut store = ns.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
    }

    let actions = exchanger.update_and_send_heartbeat(&config, &[]);
    assert!(
        actions.is_empty(),
        "接続ノードがない場合は heartbeat 送信アクション無しであるべき"
    );
}

#[test]
fn strategy_handle_message_heartbeat_is_noop_actions() {
    let strategy = make_strategy("local");
    let config = test_config();

    let utils = SpatialDensityUtils::new(config.dnve.density_resolution as usize);
    let density = utils.create_spatial_density(pos(5.0, 0.0, 0.0), &[], 2, 0.0);
    let payload = bincode::serialize(&density).unwrap();

    let actions = strategy.handle_message(node("peer-a"), OVERLAY_MSG_HEARTBEAT, &payload);
    assert!(
        actions.is_empty(),
        "HEARTBEAT の handle_message はアクションを返さないはず"
    );
}

#[test]
fn strategy_handle_message_ping_returns_pong() {
    let strategy = make_strategy("local");
    let payload: Vec<u8> = 9999u64.to_le_bytes().to_vec();
    let actions = strategy.handle_message(node("peer"), OVERLAY_MSG_PING, &payload);
    assert!(!actions.is_empty(), "PING には PONG アクションが返るべき");
}

#[test]
fn strategy_handle_message_pong_is_noop() {
    let strategy = make_strategy("local");
    let payload: Vec<u8> = 9999u64.to_le_bytes().to_vec();
    let actions = strategy.handle_message(node("peer"), OVERLAY_MSG_PONG, &payload);
    assert!(
        actions.is_empty(),
        "PONG の handle_message はアクションを返さないはず"
    );
}

#[test]
fn strategy_handle_message_unknown_type_is_noop() {
    let strategy = make_strategy("local");
    let actions = strategy.handle_message(node("peer"), 9999, b"anything");
    assert!(
        actions.is_empty(),
        "未知のメッセージタイプはアクション無しで return するべき"
    );
}

#[test]
fn strategy_tick_empty_world_returns_heartbeat_only() {
    let config = test_config();
    let strategy = make_strategy("local");

    {
        let mut store = strategy.node_store.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
    }

    let actions = strategy.tick(&config, &[]);

    let heartbeat_count = actions
        .iter()
        .filter(|a| {
            matches!(
                a,
                crate::action::OverlayAction::SendMessage { data, .. }
                if bincode::deserialize::<crate::signaling::SignalingEnvelope>(data)
                    .ok()
                    .and_then(|e| match e.content {
                        crate::signaling::MessageContent::Overlay(msg) => Some(msg.message_type),
                        _ => None,
                    })
                    == Some(OVERLAY_MSG_HEARTBEAT)
            )
        })
        .count();
    assert_eq!(
        heartbeat_count, 0,
        "接続ノードがゼロの tick では heartbeat 送信は生成されないはず"
    );
}

#[test]
fn strategy_tick_with_world_nodes_generates_connect_actions() {
    let mut config = test_config();
    config.dnve.density_resolution = 1;
    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new(60)));
    let strategy = DNVE3Strategy::new(&config, node_store, node("local"), routing_table);

    {
        let mut store = strategy.node_store.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
        store.nodes.insert(
            node("far-peer"),
            NodeInfo {
                id: node("far-peer"),
                position: pos(0.0, 0.0, 50.0),
            },
        );
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    let mut has_connect_or_send = false;

    while std::time::Instant::now() < deadline {
        let actions = strategy.tick(&config, &[]);
        has_connect_or_send = actions.iter().any(|a| {
            matches!(
                a,
                crate::action::OverlayAction::Connect { .. }
                    | crate::action::OverlayAction::SendMessage { .. }
            )
        });

        if has_connect_or_send {
            break;
        }

        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    assert!(
        has_connect_or_send,
        "near ノードがあれば tick で Connect と SendMessage が生成されるべき"
    );
}
