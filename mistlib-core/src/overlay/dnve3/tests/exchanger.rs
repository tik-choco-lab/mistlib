use super::common::{extract_node_list_payload, make_exchanger, node, pos, test_config};
use crate::action::OverlayAction;
use crate::config::{ConnectionMode, DensityEncoding, SpatialPartitionType};
use crate::overlay::dnve3::data_store::DNVE3DataStore;
use crate::overlay::dnve3::exchanger::{DNVE3Exchanger, HeartbeatDensityPayload};
use crate::overlay::dnve3::spatial_density::SpatialDensityUtils;
use crate::overlay::node_store::{NodeInfo, NodeStore};
use crate::overlay::routing_table::RoutingTable;
use crate::overlay::{OverlayEnvelope, OVERLAY_MSG_NODE_LIST, OVERLAY_MSG_REQUEST_NODE_LIST};
use crate::signaling::MessageContent;
use crate::stats::ping;
use crate::types::NodeId;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

fn send_message_envelope(action: &OverlayAction) -> OverlayEnvelope {
    let OverlayAction::SendMessage { data, .. } = action else {
        panic!("expected SendMessage action");
    };
    bincode::deserialize(data).expect("SendMessage data should be a valid overlay envelope")
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
        store.density_peers.contains_key(&node("peer-a")),
        "handle_heartbeat 後の data_store に density_peer が登録されるべき"
    );
}

#[test]
fn exchanger_heartbeat_byte_payload_roundtrip_is_accepted() {
    let mut config = test_config();
    config.dnve.density_encoding = DensityEncoding::Byte;

    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
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
        OverlayAction::SendMessage { data, .. } => {
            let env: OverlayEnvelope = bincode::deserialize(data).unwrap();
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
        store.density_peers.contains_key(&node("peer-a")),
        "byte heartbeat payload should be accepted by receiver"
    );
}

#[test]
fn exchanger_heartbeat_uses_configured_spatial_partition_type() {
    let mut config = test_config();
    config.dnve.spatial_partition_type = SpatialPartitionType::Icosahedron;
    config.dnve.density_resolution = 6;
    config.dnve.distance_layers = 3;
    config.dnve.density_encoding = DensityEncoding::Float;

    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt, node("local"), &config);

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
        OverlayAction::SendMessage { data, .. } => {
            let env: OverlayEnvelope = bincode::deserialize(data).unwrap();
            match env.content {
                MessageContent::Overlay(msg) => msg.payload,
                _ => vec![],
            }
        }
        _ => vec![],
    };

    let decoded: HeartbeatDensityPayload =
        bincode::deserialize(&payload).expect("float heartbeat payload should decode");
    match decoded {
        HeartbeatDensityPayload::Float(data) => {
            assert_eq!(data.dir_count, 20);
            assert_eq!(data.layer_count, 3);
            assert_eq!(data.density_map.len(), 20 * 3);
        }
        HeartbeatDensityPayload::Byte(_) => panic!("float config should emit Float payload"),
    }
}

#[test]
fn exchanger_handle_heartbeat_bad_payload_is_noop() {
    let (exchanger, ds, _) = make_exchanger("local");

    exchanger.handle_heartbeat(node("peer-x"), b"garbage-data");
    let store = ds.lock().unwrap();
    assert!(
        store.density_peers.is_empty(),
        "不正な payload は無視されるべき、density_peers は空のまま (パニックしてはいけない)"
    );
}

#[test]
fn exchanger_delete_old_data_removes_expired_density_peers() {
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
    fast_config.limits.expire_node_seconds = 0.0;

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
        store.density_peers.is_empty(),
        "expire_node_seconds=0 ならすべての density_peer が削除されるべき"
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
fn exchanger_send_node_list_push_returns_node_list_action() {
    let config = test_config();
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt.clone(), node("local"), &config);

    {
        let mut store = ns.lock().unwrap();
        store.nodes.insert(
            node("peer-a"),
            NodeInfo {
                id: node("peer-a"),
                position: pos(1.0, 0.0, 0.0),
            },
        );
    }
    rt.lock().unwrap().on_connected(node("peer-a"));

    let actions = exchanger.send_node_list_push(&node("peer-a"), ConnectionMode::NodeListProximity);
    assert_eq!(actions.len(), 1, "node_list push should produce one action");

    let env = send_message_envelope(&actions[0]);
    let MessageContent::Overlay(msg) = env.content else {
        panic!("node_list push should be an overlay message");
    };
    assert_eq!(msg.message_type, OVERLAY_MSG_NODE_LIST);
}

#[test]
fn exchanger_control_envelopes_use_configured_hop_count() {
    let mut config = test_config();
    config.limits.hop_count = 7;

    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt.clone(), node("local"), &config);

    {
        let mut store = ns.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
        store.nodes.insert(
            node("requester"),
            NodeInfo {
                id: node("requester"),
                position: pos(1.0, 0.0, 0.0),
            },
        );
    }

    let heartbeat = exchanger
        .update_and_send_heartbeat(&config, &[node("peer-a")])
        .pop()
        .expect("heartbeat action should be produced");
    assert_eq!(send_message_envelope(&heartbeat).hop_count, 7);

    let request = exchanger
        .send_request_node_list(&node("peer-a"))
        .pop()
        .expect("request_node_list action should be produced");
    let request_env = send_message_envelope(&request);
    assert_eq!(request_env.hop_count, 7);
    let MessageContent::Overlay(request_msg) = request_env.content else {
        panic!("request_node_list should be an overlay message");
    };
    assert_eq!(request_msg.message_type, OVERLAY_MSG_REQUEST_NODE_LIST);
    let requester_pos: crate::overlay::dnve3::spatial_density::Vector3 =
        bincode::deserialize(&request_msg.payload)
            .expect("request_node_list should carry requester position");
    assert_eq!(requester_pos, pos(0.0, 0.0, 0.0));

    rt.lock().unwrap().on_connected(node("peer-a"));
    let response = exchanger
        .handle_request_node_list(node("requester"), ConnectionMode::DirectionDensity)
        .pop()
        .expect("node_list response action should be produced");
    let response_env = send_message_envelope(&response);
    assert_eq!(response_env.hop_count, 7);
    let MessageContent::Overlay(response_msg) = response_env.content else {
        panic!("node_list response should be an overlay message");
    };
    assert_eq!(response_msg.message_type, OVERLAY_MSG_NODE_LIST);

    let pong = ping::handle_ping(&node("local"), node("sender"), 7, &12345u64.to_le_bytes())
        .pop()
        .expect("pong action should be produced");
    assert_eq!(send_message_envelope(&pong).hop_count, 7);
}

#[test]
fn exchanger_request_node_list_payload_updates_requester_position_for_aoi_extra() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListAoiGuard;
    config.dnve.aoi_range = 15.0;

    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt, node("local"), &config);

    {
        let mut store = ns.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(50.0, 0.0, 0.0),
            },
        );
        store.nodes.insert(
            node("requester"),
            NodeInfo {
                id: node("requester"),
                position: pos(100.0, 0.0, 0.0),
            },
        );
        store.nodes.insert(
            node("fresh-aoi"),
            NodeInfo {
                id: node("fresh-aoi"),
                position: pos(10.0, 0.0, 0.0),
            },
        );
    }

    let payload = bincode::serialize(&pos(0.0, 0.0, 0.0)).unwrap();
    let actions = exchanger.handle_request_node_list_with_payload(
        node("requester"),
        ConnectionMode::NodeListAoiGuard,
        &payload,
    );
    assert_eq!(
        ns.lock()
            .unwrap()
            .nodes
            .get(&node("requester"))
            .map(|n| n.position),
        Some(pos(0.0, 0.0, 0.0)),
        "REQUEST_NODE_LIST payload should refresh requester position before AOI selection"
    );

    let nodes: Vec<NodeInfo> = bincode::deserialize(&extract_node_list_payload(&actions[0]))
        .expect("node-list payload should deserialize");
    let ids: HashSet<_> = nodes.into_iter().map(|n| n.id).collect();
    assert!(
        ids.contains(&node("fresh-aoi")),
        "AOI node-list response should use requester position from request payload"
    );
}

#[test]
fn exchanger_proximity_node_list_is_limited_to_connected_nodes() {
    let config = test_config();
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt.clone(), node("local"), &config);

    {
        let mut store = ns.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(1.0, 2.0, 3.0),
            },
        );
        store.nodes.insert(
            node("known-direct"),
            NodeInfo {
                id: node("known-direct"),
                position: pos(10.0, 0.0, 0.0),
            },
        );
        store.nodes.insert(
            node("known-indirect"),
            NodeInfo {
                id: node("known-indirect"),
                position: pos(20.0, 0.0, 0.0),
            },
        );
    }
    rt.lock().unwrap().on_connected(node("known-direct"));

    let actions =
        exchanger.handle_request_node_list(node("peer-a"), ConnectionMode::NodeListProximity);
    assert_eq!(
        actions.len(),
        1,
        "node_list response should contain one action"
    );

    let nodes: Vec<NodeInfo> =
        bincode::deserialize(&extract_node_list_payload(&actions[0])).unwrap();
    let local = nodes
        .iter()
        .find(|n| n.id == node("local"))
        .expect("NodeListProximity should include sender itself");
    assert_eq!(local.position, pos(1.0, 2.0, 3.0));
    let ids: HashSet<_> = nodes.into_iter().map(|n| n.id).collect();

    assert!(
        ids.contains(&node("local")),
        "NodeListProximity should include sender itself"
    );
    assert!(
        ids.contains(&node("known-direct")),
        "NodeListProximity should include directly connected known nodes"
    );
    assert!(
        !ids.contains(&node("known-indirect")),
        "NodeListProximity should not exchange all indirectly known nodes"
    );
}

#[test]
fn exchanger_directional_node_list_is_limited_to_connected_nodes() {
    let config = test_config();
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt.clone(), node("local"), &config);

    {
        let mut store = ns.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(1.0, 2.0, 3.0),
            },
        );
        store.nodes.insert(
            node("known-direct"),
            NodeInfo {
                id: node("known-direct"),
                position: pos(10.0, 0.0, 0.0),
            },
        );
        store.nodes.insert(
            node("known-indirect"),
            NodeInfo {
                id: node("known-indirect"),
                position: pos(20.0, 0.0, 0.0),
            },
        );
    }
    rt.lock().unwrap().on_connected(node("known-direct"));

    let actions =
        exchanger.handle_request_node_list(node("peer-a"), ConnectionMode::NodeListDirectional);
    assert_eq!(
        actions.len(),
        1,
        "node_list response should contain one action"
    );

    let nodes: Vec<NodeInfo> =
        bincode::deserialize(&extract_node_list_payload(&actions[0])).unwrap();
    let local = nodes
        .iter()
        .find(|n| n.id == node("local"))
        .expect("NodeListDirectional should include sender itself");
    assert_eq!(local.position, pos(1.0, 2.0, 3.0));
    let ids: HashSet<_> = nodes.into_iter().map(|n| n.id).collect();

    assert!(
        ids.contains(&node("local")),
        "NodeListDirectional should include sender itself"
    );
    assert!(
        ids.contains(&node("known-direct")),
        "NodeListDirectional should include directly connected known nodes"
    );
    assert!(
        !ids.contains(&node("known-indirect")),
        "NodeListDirectional should not exchange all indirectly known nodes"
    );
}

#[test]
fn exchanger_direction_density_light_node_list_matches_directional_scope() {
    let config = test_config();
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt.clone(), node("local"), &config);

    {
        let mut store = ns.lock().unwrap();
        store.nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(1.0, 2.0, 3.0),
            },
        );
        store.nodes.insert(
            node("known-direct"),
            NodeInfo {
                id: node("known-direct"),
                position: pos(10.0, 0.0, 0.0),
            },
        );
        store.nodes.insert(
            node("known-indirect"),
            NodeInfo {
                id: node("known-indirect"),
                position: pos(20.0, 0.0, 0.0),
            },
        );
    }
    rt.lock().unwrap().on_connected(node("known-direct"));

    let actions =
        exchanger.handle_request_node_list(node("peer-a"), ConnectionMode::DirectionDensityLight);
    assert_eq!(
        actions.len(),
        1,
        "node_list response should contain one action"
    );

    let nodes: Vec<NodeInfo> =
        bincode::deserialize(&extract_node_list_payload(&actions[0])).unwrap();
    let ids: HashSet<_> = nodes.into_iter().map(|n| n.id).collect();

    assert!(
        ids.contains(&node("local")),
        "DirectionDensityLight should include sender itself like NodeListDirectional"
    );
    assert!(
        ids.contains(&node("known-direct")),
        "DirectionDensityLight should include directly connected known nodes"
    );
    assert!(
        !ids.contains(&node("known-indirect")),
        "DirectionDensityLight should not add DirectionDensity's requester-AOI extras"
    );
}

#[test]
fn exchanger_node_list_payload_is_limited_to_128_nodes() {
    let config = test_config();
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt.clone(), node("local"), &config);

    {
        let mut store = ns.lock().unwrap();
        for i in 0..140 {
            let id = node(&format!("peer-{i:03}"));
            store.nodes.insert(
                id.clone(),
                NodeInfo {
                    id: id.clone(),
                    position: pos(i as f32, 0.0, 0.0),
                },
            );
            rt.lock().unwrap().on_connected(id);
        }
    }

    let action = exchanger
        .send_node_list_push(&node("target"), ConnectionMode::NodeListDirectional)
        .pop()
        .expect("node_list push should be produced");
    let nodes: Vec<NodeInfo> = bincode::deserialize(&extract_node_list_payload(&action)).unwrap();

    assert_eq!(nodes.len(), 128, "node_list payload should be capped");
}

#[test]
fn exchanger_apply_node_list_is_limited_to_128_nodes() {
    let config = test_config();
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt, node("local"), &config);

    {
        ns.lock().unwrap().nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
    }

    let nodes = (0..140)
        .map(|i| NodeInfo {
            id: node(&format!("peer-{i:03}")),
            position: pos(i as f32, 0.0, 0.0),
        })
        .collect::<Vec<_>>();
    let payload = bincode::serialize(&nodes).unwrap();

    exchanger.handle_node_list(&node("source"), &payload);

    assert_eq!(
        ns.lock().unwrap().nodes.len(),
        129,
        "local node plus at most 128 node-list entries should be stored"
    );
}

#[test]
fn exchanger_apply_node_list_does_not_overwrite_local_position() {
    let config = test_config();
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt, node("local"), &config);

    {
        ns.lock().unwrap().nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(1.0, 2.0, 3.0),
            },
        );
    }

    let nodes = vec![
        NodeInfo {
            id: node("local"),
            position: pos(100.0, 100.0, 100.0),
        },
        NodeInfo {
            id: node("peer-a"),
            position: pos(4.0, 5.0, 6.0),
        },
    ];
    let payload = bincode::serialize(&nodes).unwrap();

    exchanger.handle_node_list(&node("source"), &payload);

    let store = ns.lock().unwrap();
    assert_eq!(
        store.nodes.get(&node("local")).unwrap().position,
        pos(1.0, 2.0, 3.0),
        "node-list entries for the local node must not overwrite local position"
    );
    assert_eq!(
        store.nodes.get(&node("peer-a")).unwrap().position,
        pos(4.0, 5.0, 6.0),
        "non-local node-list entries should still be applied"
    );
}

#[test]
fn exchanger_apply_node_list_ignores_broadcast_node_id() {
    let config = test_config();
    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt.clone(), node("local"), &config);

    rt.lock().unwrap().on_connected(node("source"));

    {
        ns.lock().unwrap().nodes.insert(
            node("local"),
            NodeInfo {
                id: node("local"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
    }

    let nodes = vec![
        NodeInfo {
            id: NodeId::broadcast(),
            position: pos(1.0, 2.0, 3.0),
        },
        NodeInfo {
            id: node("peer-a"),
            position: pos(4.0, 5.0, 6.0),
        },
    ];
    let payload = bincode::serialize(&nodes).unwrap();

    exchanger.handle_node_list(&node("source"), &payload);

    let store = ns.lock().unwrap();
    assert!(
        !store.nodes.contains_key(&NodeId::broadcast()),
        "node-list entries with the broadcast/empty node id must be ignored"
    );
    assert!(
        store.nodes.contains_key(&node("peer-a")),
        "valid node-list entries should still be applied"
    );
    drop(store);

    let routing_table = rt.lock().unwrap();
    assert_eq!(
        routing_table.get_next_hop(&NodeId::broadcast()),
        None,
        "broadcast/empty node id must not be learned as a routed node"
    );
    assert_eq!(
        routing_table.get_next_hop(&node("peer-a")),
        Some(node("source")),
        "valid node-list entries should still learn a route through the sender"
    );
}

#[test]
fn exchanger_direction_density_node_list_includes_requester_aoi_nodes() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::DirectionDensity;
    config.dnve.aoi_range = 25.0;

    let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
    let ns = Arc::new(Mutex::new(NodeStore::new()));
    let rt = Arc::new(Mutex::new(RoutingTable::new()));
    let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt.clone(), node("local"), &config);

    {
        let mut store = ns.lock().unwrap();
        store.nodes.insert(
            node("requester"),
            NodeInfo {
                id: node("requester"),
                position: pos(0.0, 0.0, 0.0),
            },
        );
        store.nodes.insert(
            node("known-direct"),
            NodeInfo {
                id: node("known-direct"),
                position: pos(10.0, 0.0, 0.0),
            },
        );
        store.nodes.insert(
            node("aoi-indirect"),
            NodeInfo {
                id: node("aoi-indirect"),
                position: pos(20.0, 0.0, 0.0),
            },
        );
        store.nodes.insert(
            node("far-indirect"),
            NodeInfo {
                id: node("far-indirect"),
                position: pos(100.0, 0.0, 0.0),
            },
        );
    }
    rt.lock().unwrap().on_connected(node("known-direct"));

    let actions =
        exchanger.handle_request_node_list(node("requester"), ConnectionMode::DirectionDensity);
    assert_eq!(
        actions.len(),
        1,
        "node_list response should contain one action"
    );

    let nodes: Vec<NodeInfo> =
        bincode::deserialize(&extract_node_list_payload(&actions[0])).unwrap();
    let ids: HashSet<_> = nodes.into_iter().map(|n| n.id).collect();

    assert!(
        ids.contains(&node("known-direct")),
        "DirectionDensity should still include directly connected known nodes"
    );
    assert!(
        ids.contains(&node("aoi-indirect")),
        "DirectionDensity should include known nodes inside the requester's AOI"
    );
    assert!(
        !ids.contains(&node("far-indirect")),
        "DirectionDensity should not include far indirectly known nodes"
    );
}

#[test]
fn exchanger_node_list_aoi_modes_include_self_direct_and_requester_aoi_nodes() {
    for mode in [
        ConnectionMode::NodeListAoiGuard,
        ConnectionMode::NodeListAoiProximity,
        ConnectionMode::NodeListAoiDensity,
        ConnectionMode::PSense,
    ] {
        let mut config = test_config();
        config.dnve.connection_mode = mode;
        config.dnve.aoi_range = 25.0;

        let ds = Arc::new(Mutex::new(DNVE3DataStore::new()));
        let ns = Arc::new(Mutex::new(NodeStore::new()));
        let rt = Arc::new(Mutex::new(RoutingTable::new()));
        let exchanger = DNVE3Exchanger::new(ds, ns.clone(), rt.clone(), node("local"), &config);

        {
            let mut store = ns.lock().unwrap();
            store.nodes.insert(
                node("local"),
                NodeInfo {
                    id: node("local"),
                    position: pos(1.0, 2.0, 3.0),
                },
            );
            store.nodes.insert(
                node("requester"),
                NodeInfo {
                    id: node("requester"),
                    position: pos(0.0, 0.0, 0.0),
                },
            );
            store.nodes.insert(
                node("known-direct"),
                NodeInfo {
                    id: node("known-direct"),
                    position: pos(10.0, 0.0, 0.0),
                },
            );
            store.nodes.insert(
                node("aoi-indirect"),
                NodeInfo {
                    id: node("aoi-indirect"),
                    position: pos(20.0, 0.0, 0.0),
                },
            );
            store.nodes.insert(
                node("far-indirect"),
                NodeInfo {
                    id: node("far-indirect"),
                    position: pos(100.0, 0.0, 0.0),
                },
            );
        }
        rt.lock().unwrap().on_connected(node("known-direct"));

        let actions = exchanger.handle_request_node_list(node("requester"), mode);
        assert_eq!(
            actions.len(),
            1,
            "node_list response should contain one action"
        );

        let nodes: Vec<NodeInfo> =
            bincode::deserialize(&extract_node_list_payload(&actions[0])).unwrap();
        let local = nodes
            .iter()
            .find(|n| n.id == node("local"))
            .expect("AOI node-list mode should include sender itself");
        assert_eq!(local.position, pos(1.0, 2.0, 3.0));
        let ids: HashSet<_> = nodes.into_iter().map(|n| n.id).collect();

        assert!(
            ids.contains(&node("known-direct")),
            "{mode:?} should include directly connected known nodes"
        );
        assert!(
            ids.contains(&node("aoi-indirect")),
            "{mode:?} should include known nodes inside the requester's AOI"
        );
        assert!(
            !ids.contains(&node("far-indirect")),
            "{mode:?} should not include far indirectly known nodes"
        );
    }
}

#[test]
fn exchanger_position_updates_node_position_without_density_peer() {
    let (exchanger, ds, ns) = make_exchanger("local");
    let payload = bincode::serialize(&pos(7.0, 8.0, 9.0)).unwrap();

    exchanger.handle_position(node("peer-pos"), &payload);

    let store = ns.lock().unwrap();
    let info = store
        .nodes
        .get(&node("peer-pos"))
        .expect("legacy POSITION should update node_store");
    assert_eq!(info.position, pos(7.0, 8.0, 9.0));
    assert!(
        ds.lock().unwrap().density_peers.is_empty(),
        "legacy POSITION should not create density peers"
    );
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
    let sent_to: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::SendMessage { to, .. } => Some(to.clone()),
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
