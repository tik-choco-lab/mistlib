use super::common::{make_strategy, node, pos, test_config};
use crate::action::OverlayAction;
use crate::config::{ConnectionMode, NodeListExchangeMode};
use crate::overlay::dnve3::spatial_density::SpatialDensityUtils;
use crate::overlay::dnve3::strategy::DNVE3Strategy;
use crate::overlay::node_store::{NodeInfo, NodeStore};
use crate::overlay::routing_table::RoutingTable;
use crate::overlay::{
    OverlayEnvelope, TopologyStrategy, OVERLAY_MSG_HEARTBEAT, OVERLAY_MSG_NODE_LIST,
    OVERLAY_MSG_PING, OVERLAY_MSG_PONG, OVERLAY_MSG_POSITION, OVERLAY_MSG_REQUEST_NODE_LIST,
};
use crate::signaling::MessageContent;
use crate::types::ConnectionState;
use std::sync::{Arc, Mutex};

#[test]
fn strategy_handle_message_heartbeat_is_noop_actions() {
    let strategy = make_strategy("local");
    let config = test_config();

    let utils = SpatialDensityUtils::new(config.dnve.density_resolution as usize);
    let density = utils.create_spatial_density(pos(5.0, 0.0, 0.0), &[], 2, 0.0);
    let payload = bincode::serialize(&density).unwrap();

    let peer_a = node("peer-a");
    let actions = strategy.handle_message(&peer_a, OVERLAY_MSG_HEARTBEAT, &payload);
    assert!(
        actions.is_empty(),
        "HEARTBEAT の handle_message はアクションを返さないはず"
    );
}

#[test]
fn strategy_handle_message_ping_returns_pong() {
    let strategy = make_strategy("local");
    let payload = 9999u64.to_le_bytes().to_vec();
    let peer = node("peer");
    let actions = strategy.handle_message(&peer, OVERLAY_MSG_PING, &payload);
    assert!(!actions.is_empty(), "PING には PONG アクションが返るべき");
}

#[test]
fn strategy_handle_message_pong_is_noop() {
    let strategy = make_strategy("local");
    let payload = 9999u64.to_le_bytes().to_vec();
    let peer = node("peer");
    let actions = strategy.handle_message(&peer, OVERLAY_MSG_PONG, &payload);
    assert!(
        actions.is_empty(),
        "PONG の handle_message はアクションを返さないはず"
    );
}

#[test]
fn strategy_handle_message_unknown_type_is_noop() {
    let strategy = make_strategy("local");
    let peer = node("peer");
    let actions = strategy.handle_message(&peer, 9999, b"anything");
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
                OverlayAction::SendMessage { data, .. }
                if bincode::deserialize::<OverlayEnvelope>(data)
                    .ok()
                    .and_then(|e| match e.content {
                        MessageContent::Overlay(msg) => Some(msg.message_type),
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
fn strategy_nodelist_proximity_first_tick_generates_connect_action() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListProximity;
    config.limits.max_connection_count = 2;
    config.limits.reserved_connection_count = 0;

    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
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
            node("near-peer"),
            NodeInfo {
                id: node("near-peer"),
                position: pos(2.0, 0.0, 0.0),
            },
        );
    }

    let actions = strategy.tick(&config, &[]);

    assert!(
        actions
            .iter()
            .any(|a| matches!(a, OverlayAction::Connect { to } if *to == node("near-peer"))),
        "NodeListProximity は既知ノードがあれば初回 tick で接続試行を生成すべき"
    );
}

#[test]
fn strategy_node_list_modes_push_node_list_instead_of_requesting() {
    for mode in [
        ConnectionMode::DirectionDensityLight,
        ConnectionMode::NodeListDirectional,
        ConnectionMode::NodeListAoiGuard,
        ConnectionMode::NodeListAoiProximity,
        ConnectionMode::NodeListAoiDensity,
        ConnectionMode::NodeListProximity,
        ConnectionMode::PSense,
    ] {
        let mut config = test_config();
        config.dnve.connection_mode = mode;
        config.dnve.node_list_exchange_mode = NodeListExchangeMode::Push;
        config.intervals.node_list = 0.0;

        let node_store = Arc::new(Mutex::new(NodeStore::new()));
        let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
        let strategy = DNVE3Strategy::new(
            &config,
            node_store.clone(),
            node("local"),
            routing_table.clone(),
        );

        {
            let mut store = node_store.lock().unwrap();
            store.nodes.insert(
                node("local"),
                NodeInfo {
                    id: node("local"),
                    position: pos(0.0, 0.0, 0.0),
                },
            );
            store.nodes.insert(
                node("peer-a"),
                NodeInfo {
                    id: node("peer-a"),
                    position: pos(1.0, 0.0, 0.0),
                },
            );
        }
        routing_table.lock().unwrap().on_connected(node("peer-a"));

        let connected = vec![(node("peer-a"), ConnectionState::Connected)];
        let message_types: Vec<_> = strategy
            .tick(&config, &connected)
            .iter()
            .filter_map(|action| match action {
                OverlayAction::SendMessage { data, .. } => {
                    let env: OverlayEnvelope = bincode::deserialize(data).ok()?;
                    match env.content {
                        MessageContent::Overlay(msg) => Some(msg.message_type),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect();

        assert!(
            message_types.contains(&OVERLAY_MSG_NODE_LIST),
            "{mode:?} should push NODE_LIST on node-list tick"
        );
        assert!(
            !message_types.contains(&OVERLAY_MSG_POSITION),
            "{mode:?} should carry self position in NODE_LIST instead of POSITION"
        );
        assert!(
            !message_types.contains(&OVERLAY_MSG_REQUEST_NODE_LIST),
            "{mode:?} should not send REQUEST_NODE_LIST on node-list tick"
        );
    }
}

#[test]
fn strategy_node_list_modes_pull_request_by_default() {
    for mode in [
        ConnectionMode::DirectionDensityLight,
        ConnectionMode::NodeListDirectional,
        ConnectionMode::NodeListAoiGuard,
        ConnectionMode::NodeListAoiProximity,
        ConnectionMode::NodeListAoiDensity,
        ConnectionMode::NodeListProximity,
        ConnectionMode::PSense,
    ] {
        let mut config = test_config();
        config.dnve.connection_mode = mode;
        config.intervals.node_list = 0.0;

        let node_store = Arc::new(Mutex::new(NodeStore::new()));
        let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
        let strategy = DNVE3Strategy::new(
            &config,
            node_store.clone(),
            node("local"),
            routing_table.clone(),
        );

        {
            let mut store = node_store.lock().unwrap();
            store.nodes.insert(
                node("local"),
                NodeInfo {
                    id: node("local"),
                    position: pos(0.0, 0.0, 0.0),
                },
            );
            store.nodes.insert(
                node("peer-a"),
                NodeInfo {
                    id: node("peer-a"),
                    position: pos(1.0, 0.0, 0.0),
                },
            );
        }
        routing_table.lock().unwrap().on_connected(node("peer-a"));

        let connected = vec![(node("peer-a"), ConnectionState::Connected)];
        let message_types: Vec<_> = strategy
            .tick(&config, &connected)
            .iter()
            .filter_map(|action| match action {
                OverlayAction::SendMessage { data, .. } => {
                    let env: OverlayEnvelope = bincode::deserialize(data).ok()?;
                    match env.content {
                        MessageContent::Overlay(msg) => Some(msg.message_type),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect();

        assert!(
            message_types.contains(&OVERLAY_MSG_REQUEST_NODE_LIST),
            "{mode:?} should send REQUEST_NODE_LIST by default"
        );
        assert!(
            !message_types.contains(&OVERLAY_MSG_NODE_LIST),
            "{mode:?} should not push NODE_LIST in pull mode"
        );
    }
}

#[test]
fn strategy_node_list_aoi_proximity_does_not_send_density_heartbeat() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListAoiProximity;
    config.intervals.heartbeat = 0.0;
    config.intervals.node_list = 60.0;

    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
    let strategy = DNVE3Strategy::new(&config, node_store.clone(), node("local"), routing_table);

    {
        let mut store = node_store.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
        store.nodes.insert(
            node("peer-a"),
            NodeInfo {
                id: node("peer-a"),
                position: pos(1.0, 0.0, 0.0),
            },
        );
    }

    let connected = vec![(node("peer-a"), ConnectionState::Connected)];
    let sends_heartbeat = strategy.tick(&config, &connected).iter().any(|action| {
        matches!(
            action,
            OverlayAction::SendMessage { data, .. }
            if bincode::deserialize::<OverlayEnvelope>(data)
                .ok()
                .and_then(|env| match env.content {
                    MessageContent::Overlay(msg) => Some(msg.message_type),
                    _ => None,
                })
                == Some(OVERLAY_MSG_HEARTBEAT)
        )
    });

    assert!(
        !sends_heartbeat,
        "NodeListAoiProximity should stay node-list only and not send density HEARTBEAT"
    );
}

#[test]
fn strategy_nodelist_directional_mode_processes_position_heartbeat() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListDirectional;

    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
    let strategy = DNVE3Strategy::new(&config, node_store.clone(), node("local"), routing_table);

    strategy.tick(&config, &[]);

    let payload = bincode::serialize(&pos(11.0, 12.0, 13.0)).unwrap();
    let actions = strategy.handle_message(&node("peer-pos"), OVERLAY_MSG_POSITION, &payload);

    assert!(
        actions.is_empty(),
        "POSITION handle_message should not emit actions"
    );
    let store = node_store.lock().unwrap();
    let info = store
        .nodes
        .get(&node("peer-pos"))
        .expect("NodeListDirectional should still accept legacy POSITION messages");
    assert_eq!(info.position, pos(11.0, 12.0, 13.0));
}

#[test]
fn strategy_node_list_aoi_guard_mode_processes_position_heartbeat() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListAoiGuard;

    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
    let strategy = DNVE3Strategy::new(&config, node_store.clone(), node("local"), routing_table);

    strategy.tick(&config, &[]);

    let payload = bincode::serialize(&pos(14.0, 15.0, 16.0)).unwrap();
    let actions = strategy.handle_message(&node("peer-pos"), OVERLAY_MSG_POSITION, &payload);

    assert!(
        actions.is_empty(),
        "POSITION handle_message should not emit actions"
    );
    let store = node_store.lock().unwrap();
    let info = store
        .nodes
        .get(&node("peer-pos"))
        .expect("NodeListAoiGuard should still accept legacy POSITION messages");
    assert_eq!(info.position, pos(14.0, 15.0, 16.0));
}

#[test]
fn strategy_node_list_aoi_proximity_mode_processes_position_heartbeat() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListAoiProximity;

    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
    let strategy = DNVE3Strategy::new(&config, node_store.clone(), node("local"), routing_table);

    strategy.tick(&config, &[]);

    let payload = bincode::serialize(&pos(17.0, 18.0, 19.0)).unwrap();
    let actions = strategy.handle_message(&node("peer-pos"), OVERLAY_MSG_POSITION, &payload);

    assert!(
        actions.is_empty(),
        "POSITION handle_message should not emit actions"
    );
    let store = node_store.lock().unwrap();
    let info = store
        .nodes
        .get(&node("peer-pos"))
        .expect("NodeListAoiProximity should still accept legacy POSITION messages");
    assert_eq!(info.position, pos(17.0, 18.0, 19.0));
}

#[test]
fn strategy_nodelist_proximity_mode_processes_position_heartbeat() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListProximity;

    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
    let strategy = DNVE3Strategy::new(&config, node_store.clone(), node("local"), routing_table);

    strategy.tick(&config, &[]);

    let payload = bincode::serialize(&pos(21.0, 22.0, 23.0)).unwrap();
    let actions = strategy.handle_message(&node("peer-pos"), OVERLAY_MSG_POSITION, &payload);

    assert!(
        actions.is_empty(),
        "POSITION handle_message should not emit actions"
    );
    let store = node_store.lock().unwrap();
    let info = store
        .nodes
        .get(&node("peer-pos"))
        .expect("NodeListProximity should still accept legacy POSITION messages");
    assert_eq!(info.position, pos(21.0, 22.0, 23.0));
}

#[test]
fn strategy_direction_density_mode_processes_incoming_heartbeat() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::DirectionDensity;

    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
    let strategy = DNVE3Strategy::new(&config, node_store, node("local"), routing_table);

    strategy.tick(&config, &[]);

    let utils = SpatialDensityUtils::new(config.dnve.density_resolution as usize);
    let density = utils.create_spatial_density(pos(5.0, 0.0, 0.0), &[pos(10.0, 0.0, 0.0)], 2, 0.0);
    let payload = bincode::serialize(&density).unwrap();

    strategy.handle_message(&node("peer-x"), OVERLAY_MSG_HEARTBEAT, &payload);

    let store = strategy.node_store.lock().unwrap();
    assert!(
        store.nodes.contains_key(&node("peer-x")),
        "DirectionDensity モードでは HEARTBEAT 受信でノードの位置が node_store に登録されるべき"
    );
}

#[test]
fn strategy_direction_density_light_mode_processes_incoming_heartbeat() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::DirectionDensityLight;

    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
    let strategy = DNVE3Strategy::new(&config, node_store, node("local"), routing_table);

    strategy.tick(&config, &[]);

    let utils = SpatialDensityUtils::new(config.dnve.density_resolution as usize);
    let density = utils.create_spatial_density(pos(5.0, 0.0, 0.0), &[pos(10.0, 0.0, 0.0)], 2, 0.0);
    let payload = bincode::serialize(&density).unwrap();

    strategy.handle_message(&node("peer-x"), OVERLAY_MSG_HEARTBEAT, &payload);

    let store = strategy.node_store.lock().unwrap();
    assert!(
        store.nodes.contains_key(&node("peer-x")),
        "DirectionDensityLight モードでは HEARTBEAT 受信でノードの位置が node_store に登録されるべき"
    );
}

#[test]
fn strategy_node_list_aoi_density_mode_processes_incoming_heartbeat() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListAoiDensity;

    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
    let strategy = DNVE3Strategy::new(&config, node_store.clone(), node("local"), routing_table);
    let utils = SpatialDensityUtils::new(config.dnve.density_resolution as usize);
    let data = utils.create_spatial_density(pos(4.0, 5.0, 6.0), &[], 2, 0.0);

    strategy.handle_message(
        &node("peer-x"),
        OVERLAY_MSG_HEARTBEAT,
        &bincode::serialize(&data).unwrap(),
    );

    assert_eq!(
        node_store
            .lock()
            .unwrap()
            .nodes
            .get(&node("peer-x"))
            .map(|n| n.position),
        Some(pos(4.0, 5.0, 6.0)),
        "NodeListAoiDensity モードでは HEARTBEAT 受信でノードの位置が node_store に登録されるべき"
    );
}

#[test]
fn strategy_direction_density_orders_node_list_requests_by_density() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::DirectionDensity;
    config.intervals.node_list = 0.0;

    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
    let strategy = DNVE3Strategy::new(&config, node_store.clone(), node("local"), routing_table);

    {
        let mut store = node_store.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
    }

    let utils = SpatialDensityUtils::new(config.dnve.density_resolution as usize);
    let rich_density = utils.create_spatial_density(
        pos(10.0, 0.0, 0.0),
        &[pos(11.0, 0.0, 0.0), pos(12.0, 0.0, 0.0)],
        2,
        0.0,
    );
    let poor_density = utils.create_spatial_density(pos(-10.0, 0.0, 0.0), &[], 2, 0.0);

    strategy.handle_message(
        &node("rich"),
        OVERLAY_MSG_HEARTBEAT,
        &bincode::serialize(&rich_density).unwrap(),
    );
    strategy.handle_message(
        &node("poor"),
        OVERLAY_MSG_HEARTBEAT,
        &bincode::serialize(&poor_density).unwrap(),
    );

    let connected = vec![
        (node("poor"), ConnectionState::Connected),
        (node("rich"), ConnectionState::Connected),
    ];
    let actions = strategy.tick(&config, &connected);
    let request_targets: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            OverlayAction::SendMessage { to, data, .. } => {
                let env: OverlayEnvelope = bincode::deserialize(data).ok()?;
                match env.content {
                    MessageContent::Overlay(msg)
                        if msg.message_type == OVERLAY_MSG_REQUEST_NODE_LIST =>
                    {
                        Some(to.clone())
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        request_targets,
        vec![node("rich"), node("poor")],
        "DirectionDensity は density の高い peer から node-list を問い合わせるべき"
    );
}

#[test]
fn strategy_direction_density_light_keeps_directional_node_list_target_order() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::DirectionDensityLight;
    config.intervals.node_list = 0.0;

    let node_store = Arc::new(Mutex::new(NodeStore::new()));
    let routing_table = Arc::new(Mutex::new(RoutingTable::new()));
    let strategy = DNVE3Strategy::new(&config, node_store.clone(), node("local"), routing_table);

    {
        let mut store = node_store.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
    }

    let utils = SpatialDensityUtils::new(config.dnve.density_resolution as usize);
    let rich_density = utils.create_spatial_density(
        pos(10.0, 0.0, 0.0),
        &[pos(11.0, 0.0, 0.0), pos(12.0, 0.0, 0.0)],
        2,
        0.0,
    );
    let poor_density = utils.create_spatial_density(pos(-10.0, 0.0, 0.0), &[], 2, 0.0);

    strategy.handle_message(
        &node("rich"),
        OVERLAY_MSG_HEARTBEAT,
        &bincode::serialize(&rich_density).unwrap(),
    );
    strategy.handle_message(
        &node("poor"),
        OVERLAY_MSG_HEARTBEAT,
        &bincode::serialize(&poor_density).unwrap(),
    );

    let connected = vec![
        (node("poor"), ConnectionState::Connected),
        (node("rich"), ConnectionState::Connected),
    ];
    let actions = strategy.tick(&config, &connected);
    let request_targets: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            OverlayAction::SendMessage { to, data, .. } => {
                let env: OverlayEnvelope = bincode::deserialize(data).ok()?;
                match env.content {
                    MessageContent::Overlay(msg)
                        if msg.message_type == OVERLAY_MSG_REQUEST_NODE_LIST =>
                    {
                        Some(to.clone())
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        request_targets,
        vec![node("poor"), node("rich")],
        "DirectionDensityLight should keep NodeListDirectional node-list target order"
    );
}
