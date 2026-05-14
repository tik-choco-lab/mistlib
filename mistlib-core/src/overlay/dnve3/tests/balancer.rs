use super::common::{make_balancer, node, pos, test_config};
use crate::action::OverlayAction;
use crate::config::{ConnectionMode, SpatialPartitionType};
use crate::overlay::dnve3::balancer::DensityGuidance;
use crate::types::{ConnectionState, NodeId};
use std::collections::{HashMap, HashSet};

// ─────────────────────────────────────────────────────────────────────────────
// DirectionDensity and node-list balancer behavior
// ─────────────────────────────────────────────────────────────────────────────

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
        .any(|a| matches!(a, OverlayAction::Connect { to } if *to == peer));
    assert!(
        has_connect,
        "近くのノードへの Connect アクションが生成されるべき"
    );
}

#[test]
fn balancer_respects_configured_direction_threshold() {
    let self_id = node("self");
    let peer = node("peer-a");
    let all_nodes = vec![(peer.clone(), pos(1.0, 0.0, 1.0))];

    let mut strict_config = test_config();
    strict_config.dnve.connection_mode = ConnectionMode::NodeListDirectional;
    strict_config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    strict_config.dnve.density_resolution = 1;
    strict_config.dnve.direction_threshold = 0.8;
    let strict_balancer = make_balancer(&strict_config);
    let strict_actions = strict_balancer.select_connections(
        &strict_config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &[],
        &self_id,
    );

    let mut relaxed_config = test_config();
    relaxed_config.dnve.connection_mode = ConnectionMode::NodeListDirectional;
    relaxed_config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    relaxed_config.dnve.density_resolution = 1;
    relaxed_config.dnve.direction_threshold = 0.7;
    let relaxed_balancer = make_balancer(&relaxed_config);
    let relaxed_actions = relaxed_balancer.select_connections(
        &relaxed_config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &[],
        &self_id,
    );

    let strict_has_connect = strict_actions
        .iter()
        .any(|a| matches!(a, OverlayAction::Connect { to } if *to == peer));
    let relaxed_has_connect = relaxed_actions
        .iter()
        .any(|a| matches!(a, OverlayAction::Connect { to } if *to == peer));

    assert!(!strict_has_connect, "閾値が高い場合は接続されないはず");
    assert!(relaxed_has_connect, "閾値が低い場合は接続されるべき");
}

#[test]
fn balancer_cube_partition_selects_axis_direction_nodes() {
    let mut config = test_config();
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.limits.max_connection_count = 10;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let axis_peers = [
        (node("plus-x"), pos(10.0, 0.0, 0.0)),
        (node("minus-x"), pos(-10.0, 0.0, 0.0)),
        (node("plus-y"), pos(0.0, 10.0, 0.0)),
        (node("minus-y"), pos(0.0, -10.0, 0.0)),
        (node("plus-z"), pos(0.0, 0.0, 10.0)),
        (node("minus-z"), pos(0.0, 0.0, -10.0)),
    ];
    let diagonal = node("diagonal");
    let mut all_nodes = axis_peers.to_vec();
    all_nodes.push((diagonal.clone(), pos(10.0, 10.0, 10.0)));

    let actions =
        balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);
    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|action| match action {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    for (peer, _) in axis_peers {
        assert!(
            connects.contains(&peer),
            "cube partition should connect to axis peer {peer:?}"
        );
    }
    assert!(
        !connects.contains(&diagonal),
        "strict cube face-normal partition should not connect to diagonal peer"
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
        .any(|a| matches!(a, OverlayAction::Connect { to } if *to == self_id));
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

    let all_nodes: Vec<_> = connected
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
        .filter(|a| matches!(a, OverlayAction::Disconnect { .. }))
        .count();

    assert!(
        disconnect_count >= 1,
        "max を超えた場合は少なくとも 1 つの Disconnect が生成されるべき(got {disconnect_count})"
    );
}

#[test]
fn balancer_disconnects_farther_unselected_node_when_over_target() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListDirectional;
    config.dnve.direction_threshold = 0.99;
    config.limits.max_connection_count = 3;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let near_selected = node("near-selected");
    let mid = node("mid");
    let far = node("far");
    let farther = node("farther");

    let connected = vec![
        (mid.clone(), ConnectionState::Connected),
        (farther.clone(), ConnectionState::Connected),
        (near_selected.clone(), ConnectionState::Connected),
        (far.clone(), ConnectionState::Connected),
    ];
    let all_nodes = vec![
        (near_selected.clone(), pos(10.0, 0.0, 0.0)),
        (mid.clone(), pos(20.0, 0.0, 0.0)),
        (far.clone(), pos(30.0, 0.0, 0.0)),
        (farther.clone(), pos(40.0, 0.0, 0.0)),
    ];

    let actions = balancer.select_connections(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &connected,
        &self_id,
    );

    assert!(
        actions
            .iter()
            .any(|a| matches!(a, OverlayAction::Disconnect { to } if *to == farther)),
        "over capacity pruning should drop the farthest unselected node first"
    );
}

#[test]
fn balancer_keeps_directional_nearest_nodes_even_when_over_target() {
    let mut config = test_config();
    config.limits.max_connection_count = 2;
    config.limits.reserved_connection_count = 1;
    config.limits.force_disconnect_count = 0;
    config.dnve.density_resolution = 1;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let mandatory = node("mandatory");

    let connected = vec![(mandatory.clone(), ConnectionState::Connected)];
    let all_nodes = vec![(mandatory.clone(), pos(0.0, 0.0, 10.0))];

    let actions = balancer.select_connections(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &connected,
        &self_id,
    );

    let disconnect_count = actions
        .iter()
        .filter(|a| matches!(a, OverlayAction::Disconnect { .. }))
        .count();

    assert_eq!(
        disconnect_count, 0,
        "方向ごとの最短距離ノードは上限超過時でも切断対象にしないべき"
    );
}

#[test]
fn balancer_connects_directional_nearest_nodes_even_if_it_exceeds_max() {
    let mut config = test_config();
    config.limits.max_connection_count = 1;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;
    config.dnve.density_resolution = 1;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let mandatory = node("mandatory");
    let incumbent = node("incumbent");

    let connected = vec![(incumbent, ConnectionState::Connected)];
    let all_nodes = vec![(mandatory.clone(), pos(0.0, 0.0, 10.0))];

    let actions = balancer.select_connections(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &connected,
        &self_id,
    );

    let has_connect = actions
        .iter()
        .any(|a| matches!(a, OverlayAction::Connect { to } if *to == mandatory));
    let has_disconnect = actions
        .iter()
        .any(|a| matches!(a, OverlayAction::Disconnect { .. }));

    assert!(
        has_connect,
        "方向ごとの最短距離ノードは満杯でも Connect されるべき"
    );
    assert!(
        has_disconnect,
        "非必須ノードがあればそちらを切断してでも必須ノードへの接続を優先するべき"
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
        .filter(|a| matches!(a, OverlayAction::Connect { to } if *to == peer))
        .count();

    assert_eq!(
        connect_to_peer, 0,
        "既に Connected のノードへ Connect アクションは生成されないはず"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// NodeListProximity mode
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn balancer_proximity_mode_connects_closest_node() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListProximity;
    config.limits.max_connection_count = 3;
    config.limits.reserved_connection_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let close = node("close");
    let far = node("far");

    let all_nodes = vec![
        (close.clone(), pos(0.0, 2.0, 0.0)),
        (far.clone(), pos(0.0, 100.0, 0.0)),
    ];
    let actions =
        balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&close),
        "proximity モードは最も近いノードに接続すべき"
    );
    assert!(
        connects.contains(&far),
        "proximity モードは max_connection_count 未満なら遠いノードにも接続すべき"
    );
}

#[test]
fn balancer_node_list_proximity_uses_proximity_branch() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListProximity;
    config.dnve.density_resolution = 1;
    config.limits.max_connection_count = 2;
    config.limits.reserved_connection_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let near_wrong_direction = node("near-wrong-direction");
    let far_directional = node("far-directional");

    let all_nodes = vec![
        (near_wrong_direction.clone(), pos(1.0, 0.0, 0.0)),
        (far_directional.clone(), pos(0.0, 0.0, 100.0)),
    ];

    let actions =
        balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    let connects: Vec<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&near_wrong_direction),
        "NodeListProximity は方向条件を使わず、近いノードに接続すべき"
    );
    assert!(
        connects.contains(&far_directional),
        "max_connection_count 以内なら距離順で複数ノードを接続候補にすべき"
    );
}

#[test]
fn balancer_proximity_mode_respects_max_connection_count() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListProximity;
    config.limits.max_connection_count = 3;
    config.limits.reserved_connection_count = 1;

    let balancer = make_balancer(&config);
    let self_id = node("self");

    let all_nodes: Vec<_> = (0..5)
        .map(|i| {
            (
                node(&format!("peer-{i}")),
                pos((i + 1) as f32 * 3.0, 0.0, 0.0),
            )
        })
        .collect();

    let actions =
        balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    let connect_count = actions
        .iter()
        .filter(|a| matches!(a, OverlayAction::Connect { .. }))
        .count();

    assert!(
        connect_count <= 3,
        "proximity モードは max_connection_count={} を超えて接続しないべき (got {connect_count})",
        3
    );
}

#[test]
fn balancer_proximity_mode_connects_nearest_even_when_target_is_zero() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListProximity;
    config.limits.max_connection_count = 1;
    config.limits.reserved_connection_count = 1; // target = 0
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let nearest = node("nearest");
    let far = node("far");

    let all_nodes = vec![
        (far, pos(30.0, 0.0, 0.0)),
        (nearest.clone(), pos(3.0, 0.0, 0.0)),
    ];

    let actions =
        balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    assert!(
        actions
            .iter()
            .any(|a| matches!(a, OverlayAction::Connect { to } if *to == nearest)),
        "proximity モードは target=0 でも最も近いノードへの接続試行を生成すべき"
    );
}

#[test]
fn balancer_proximity_mode_selects_two_nearest_when_max_is_two() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListProximity;
    config.limits.max_connection_count = 2;
    config.limits.reserved_connection_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let nearest = node("nearest");
    let second = node("second");
    let third = node("third");

    let all_nodes = vec![
        (nearest.clone(), pos(1.0, 0.0, 0.0)),
        (second.clone(), pos(5.0, 0.0, 0.0)),
        (third.clone(), pos(50.0, 0.0, 0.0)),
    ];

    let actions =
        balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(connects.contains(&nearest), "最も近いノードに接続すべき");
    assert!(connects.contains(&second), "2 番目に近いノードに接続すべき");
    assert!(
        !connects.contains(&third),
        "3 番目以降のノードは max_connection_count=2 なら接続されないべき"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// NodeListDirectional vs DirectionDensity
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn balancer_node_list_directional_selects_same_nodes_as_direction_density() {
    let self_id = node("self");
    let all_nodes = vec![
        (node("north"), pos(0.0, 0.0, 10.0)),
        (node("east"), pos(10.0, 0.0, 0.0)),
    ];

    let mut dd_config = test_config();
    dd_config.dnve.density_resolution = 1;
    dd_config.dnve.connection_mode = ConnectionMode::DirectionDensity;
    let dd_balancer = make_balancer(&dd_config);
    let dd_actions =
        dd_balancer.select_connections(&dd_config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    let mut nl_config = test_config();
    nl_config.dnve.density_resolution = 1;
    nl_config.dnve.connection_mode = ConnectionMode::NodeListDirectional;
    let nl_balancer = make_balancer(&nl_config);
    let nl_actions =
        nl_balancer.select_connections(&nl_config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    let dd_connects: HashSet<_> = dd_actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();
    let nl_connects: HashSet<_> = nl_actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        dd_connects, nl_connects,
        "NodeListDirectional と DirectionDensity は同じノードを選択するべき"
    );
}

#[test]
fn balancer_node_list_directional_ignores_density_guidance() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListDirectional;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.limits.max_connection_count = 2;
    config.limits.reserved_connection_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let near = node("near");
    let dense_far = node("dense-far");
    let all_nodes = vec![
        (near.clone(), pos(10.0, 0.0, 0.0)),
        (dense_far.clone(), pos(100.0, 0.0, 0.0)),
    ];
    let guidance = DensityGuidance {
        direction_scores: vec![100.0; 6],
        peer_scores: HashMap::from([(dense_far.clone(), 1_000.0)]),
    };

    let actions = balancer.select_connections_with_density_guidance(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &[],
        &self_id,
        Some(&guidance),
    );

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&near),
        "NodeListDirectional は density guidance が渡されても方向ごとの最近傍を選ぶべき"
    );
    assert!(
        !connects.contains(&dense_far),
        "NodeListDirectional は density score の高い遠方ノードを優先してはいけない"
    );
}

#[test]
fn balancer_direction_density_uses_density_guidance() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::DirectionDensity;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.dnve.aoi_range = 5.0;
    config.limits.max_connection_count = 2;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let near = node("near");
    let dense_far = node("dense-far");
    let all_nodes = vec![
        (near.clone(), pos(10.0, 0.0, 0.0)),
        (dense_far.clone(), pos(100.0, 0.0, 0.0)),
    ];
    let guidance = DensityGuidance {
        direction_scores: vec![0.0; 6],
        peer_scores: HashMap::from([(dense_far.clone(), 1_000.0)]),
    };

    let actions = balancer.select_connections_with_density_guidance(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &[],
        &self_id,
        Some(&guidance),
    );

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&dense_far),
        "DirectionDensity は容量に余裕があれば density score の高い候補を追加できるべき"
    );
    assert!(
        connects.contains(&near),
        "DirectionDensity は density extra を足す場合でも baseline directional candidate を保護するべき"
    );
}

#[test]
fn balancer_direction_density_keeps_aoi_nearest_before_density_extra() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::DirectionDensity;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.dnve.aoi_range = 20.0;
    config.limits.max_connection_count = 1;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let aoi_near = node("aoi-near");
    let dense_far = node("dense-far");
    let all_nodes = vec![
        (aoi_near.clone(), pos(10.0, 0.0, 0.0)),
        (dense_far.clone(), pos(100.0, 0.0, 0.0)),
    ];
    let guidance = DensityGuidance {
        direction_scores: vec![0.0; 6],
        peer_scores: HashMap::from([(dense_far.clone(), 1_000.0)]),
    };

    let actions = balancer.select_connections_with_density_guidance(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &[],
        &self_id,
        Some(&guidance),
    );

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&aoi_near),
        "DirectionDensity should protect known AOI-nearest candidates before density extras"
    );
    assert!(
        !connects.contains(&dense_far),
        "when capacity is tight, density should not evict an AOI-nearest candidate"
    );
}

#[test]
fn balancer_direction_density_light_uses_density_without_aoi_guard() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::DirectionDensityLight;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.dnve.aoi_range = 10.0;
    config.limits.max_connection_count = 2;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let directional_near = node("directional-near");
    let dense_far = node("dense-far");
    let aoi_diagonal = node("aoi-diagonal");
    let all_nodes = vec![
        (directional_near.clone(), pos(10.0, 0.0, 0.0)),
        (dense_far.clone(), pos(100.0, 0.0, 0.0)),
        (aoi_diagonal.clone(), pos(3.0, 3.0, 0.0)),
    ];
    let guidance = DensityGuidance {
        direction_scores: vec![0.0; 6],
        peer_scores: HashMap::from([(dense_far.clone(), 1_000.0)]),
    };

    let actions = balancer.select_connections_with_density_guidance(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &[],
        &self_id,
        Some(&guidance),
    );

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&directional_near),
        "DirectionDensityLight should keep NodeListDirectional baseline candidates"
    );
    assert!(
        connects.contains(&dense_far),
        "DirectionDensityLight should add density-score candidates when capacity allows"
    );
    assert!(
        !connects.contains(&aoi_diagonal),
        "DirectionDensityLight should not add DirectionDensity's AOI guard candidates"
    );
}

#[test]
fn balancer_node_list_aoi_guard_keeps_aoi_nearest_before_directional() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListAoiGuard;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.dnve.aoi_range = 20.0;
    config.limits.max_connection_count = 1;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let aoi_near = node("aoi-near");
    let directional_far = node("directional-far");
    let all_nodes = vec![
        (aoi_near.clone(), pos(10.0, 0.0, 0.0)),
        (directional_far.clone(), pos(100.0, 0.0, 0.0)),
    ];

    let actions = balancer.select_connections_with_density_guidance(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &[],
        &self_id,
        None,
    );

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&aoi_near),
        "NodeListAoiGuard should protect AOI-nearest candidates before directional extras"
    );
    assert!(
        !connects.contains(&directional_far),
        "NodeListAoiGuard should not evict an AOI-nearest candidate when capacity is tight"
    );
}

#[test]
fn balancer_node_list_aoi_guard_does_not_protect_existing_long_range_bridge() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListAoiGuard;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.dnve.aoi_range = 10.0;
    config.limits.max_connection_count = 2;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let aoi_near = node("aoi-near");
    let disposable = node("disposable");
    let bridge = node("bridge");

    let connected = vec![
        (aoi_near.clone(), ConnectionState::Connected),
        (disposable.clone(), ConnectionState::Connected),
        (bridge.clone(), ConnectionState::Connected),
    ];
    let all_nodes = vec![
        (aoi_near.clone(), pos(5.0, 0.0, 0.0)),
        (disposable.clone(), pos(20.0, 0.0, 0.0)),
        (bridge.clone(), pos(100.0, 0.0, 0.0)),
    ];

    let actions = balancer.select_connections_with_density_guidance(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &connected,
        &self_id,
        None,
    );

    assert!(
        actions
            .iter()
            .any(|a| matches!(a, OverlayAction::Disconnect { to } if *to == bridge)),
        "NodeListAoiGuard should no longer protect existing long-range bridge links"
    );
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, OverlayAction::Disconnect { to } if *to == disposable)),
        "NodeListAoiGuard should prune by normal distance ordering once bridge guard is disabled"
    );
}

#[test]
fn balancer_node_list_aoi_guard_ignores_density_guidance() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListAoiGuard;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.dnve.aoi_range = 5.0;
    config.limits.max_connection_count = 2;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let near = node("near");
    let dense_far = node("dense-far");
    let all_nodes = vec![
        (near.clone(), pos(10.0, 0.0, 0.0)),
        (dense_far.clone(), pos(100.0, 0.0, 0.0)),
    ];
    let guidance = DensityGuidance {
        direction_scores: vec![100.0; 6],
        peer_scores: HashMap::from([(dense_far.clone(), 1_000.0)]),
    };

    let actions = balancer.select_connections_with_density_guidance(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &[],
        &self_id,
        Some(&guidance),
    );

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&near),
        "NodeListAoiGuard should keep directional nearest candidates"
    );
    assert!(
        !connects.contains(&dense_far),
        "NodeListAoiGuard should not use density guidance"
    );
}

#[test]
fn balancer_node_list_aoi_proximity_uses_proximity_after_aoi_exchange() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListAoiProximity;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.dnve.aoi_range = 0.0;
    config.limits.max_connection_count = 1;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let near_diagonal = node("near-diagonal");
    let directional_far = node("directional-far");
    let all_nodes = vec![
        (near_diagonal.clone(), pos(1.0, 1.0, 0.0)),
        (directional_far.clone(), pos(20.0, 0.0, 0.0)),
    ];

    let actions = balancer.select_connections_with_density_guidance(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &[],
        &self_id,
        None,
    );

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&near_diagonal),
        "NodeListAoiProximity should use distance order instead of directional cones"
    );
    assert!(
        !connects.contains(&directional_far),
        "NodeListAoiProximity should not prefer a farther directional candidate"
    );
}

#[test]
fn balancer_node_list_aoi_density_uses_aoi_directional_and_density_guidance() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListAoiDensity;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.dnve.aoi_range = 10.0;
    config.limits.max_connection_count = 3;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let aoi_near = node("aoi-near");
    let directional_near = node("directional-near");
    let dense_far = node("dense-far");
    let all_nodes = vec![
        (aoi_near.clone(), pos(3.0, 3.0, 0.0)),
        (directional_near.clone(), pos(10.0, 0.0, 0.0)),
        (dense_far.clone(), pos(100.0, 0.0, 0.0)),
    ];
    let guidance = DensityGuidance {
        direction_scores: vec![0.0; 6],
        peer_scores: HashMap::from([(dense_far.clone(), 1_000.0)]),
    };

    let actions = balancer.select_connections_with_density_guidance(
        &config,
        pos(0.0, 0.0, 0.0),
        &all_nodes,
        &[],
        &self_id,
        Some(&guidance),
    );

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&aoi_near),
        "NodeListAoiDensity should keep AOI guard candidates"
    );
    assert!(
        connects.contains(&directional_near),
        "NodeListAoiDensity should keep directional baseline candidates"
    );
    assert!(
        connects.contains(&dense_far),
        "NodeListAoiDensity should add density-score candidates when capacity allows"
    );
}

#[test]
fn balancer_p_sense_keeps_near_nodes_before_sensor_nodes() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::PSense;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.dnve.aoi_range = 10.0;
    config.limits.max_connection_count = 3;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let near_a = node("near-a");
    let near_b = node("near-b");
    let sensor_x = node("sensor-x");
    let sensor_y = node("sensor-y");
    let all_nodes = vec![
        (sensor_y.clone(), pos(0.0, 20.0, 0.0)),
        (near_b.clone(), pos(0.0, 9.0, 0.0)),
        (sensor_x.clone(), pos(20.0, 0.0, 0.0)),
        (near_a.clone(), pos(3.0, 0.0, 0.0)),
    ];

    let actions =
        balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&near_a),
        "pSense should keep AOI near node A"
    );
    assert!(
        connects.contains(&near_b),
        "pSense should keep AOI near node B"
    );
    assert_eq!(
        connects.len(),
        3,
        "pSense should fill remaining capacity with one sensor node"
    );
    assert!(
        connects.contains(&sensor_x) || connects.contains(&sensor_y),
        "pSense should add an outside-AOI sector sensor when capacity remains"
    );
}

#[test]
fn balancer_p_sense_selects_closest_outside_aoi_sensor_per_direction() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::PSense;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.dnve.aoi_range = 10.0;
    config.limits.max_connection_count = 4;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let inside_aoi = node("inside-aoi");
    let close_sensor = node("close-sensor");
    let far_sensor = node("far-sensor");
    let other_direction = node("other-direction");
    let all_nodes = vec![
        (inside_aoi.clone(), pos(5.0, 0.0, 0.0)),
        (far_sensor.clone(), pos(30.0, 0.0, 0.0)),
        (close_sensor.clone(), pos(12.0, 0.0, 0.0)),
        (other_direction.clone(), pos(0.0, 20.0, 0.0)),
    ];

    let actions =
        balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert!(
        connects.contains(&inside_aoi),
        "pSense should always prefer known nodes inside AOI"
    );
    assert!(
        connects.contains(&close_sensor),
        "pSense should choose the closest sensor just outside AOI in a sector"
    );
    assert!(
        !connects.contains(&far_sensor),
        "pSense should not choose a farther sensor in the same sector"
    );
    assert!(
        connects.contains(&other_direction),
        "pSense should keep sensors distributed across sectors"
    );
}

#[test]
fn balancer_p_sense_reserves_sensor_slot_when_aoi_is_dense() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::PSense;
    config.dnve.spatial_partition_type = SpatialPartitionType::Cube;
    config.dnve.direction_threshold = 0.99;
    config.dnve.aoi_range = 10.0;
    config.limits.max_connection_count = 4;
    config.limits.reserved_connection_count = 0;
    config.limits.force_disconnect_count = 0;

    let balancer = make_balancer(&config);
    let self_id = node("self");
    let near_a = node("near-a");
    let near_b = node("near-b");
    let near_c = node("near-c");
    let near_d = node("near-d");
    let sensor_x = node("sensor-x");
    let all_nodes = vec![
        (near_d.clone(), pos(4.0, 0.0, 0.0)),
        (near_c.clone(), pos(3.0, 0.0, 0.0)),
        (near_b.clone(), pos(2.0, 0.0, 0.0)),
        (near_a.clone(), pos(1.0, 0.0, 0.0)),
        (sensor_x.clone(), pos(20.0, 0.0, 0.0)),
    ];

    let actions =
        balancer.select_connections(&config, pos(0.0, 0.0, 0.0), &all_nodes, &[], &self_id);

    let connects: HashSet<_> = actions
        .iter()
        .filter_map(|a| match a {
            OverlayAction::Connect { to } => Some(to.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        connects.len(),
        4,
        "pSense should still fill all available connection slots"
    );
    assert!(
        connects.contains(&sensor_x),
        "pSense should reserve a sensor slot even when AOI nodes exceed capacity"
    );
    assert!(
        !connects.contains(&near_d),
        "pSense should evict the farthest AOI node before dropping the reserved sensor"
    );
}
