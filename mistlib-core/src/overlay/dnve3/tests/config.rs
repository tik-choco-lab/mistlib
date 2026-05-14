use crate::config::{Config, ConnectionMode, NodeListExchangeMode, SpatialPartitionType};

#[test]
fn connection_mode_parse_as_str_roundtrip() {
    for mode in [
        ConnectionMode::DirectionDensity,
        ConnectionMode::DirectionDensityLight,
        ConnectionMode::NodeListDirectional,
        ConnectionMode::NodeListAoiGuard,
        ConnectionMode::NodeListAoiProximity,
        ConnectionMode::NodeListAoiDensity,
        ConnectionMode::NodeListProximity,
        ConnectionMode::PSense,
    ] {
        let parsed = ConnectionMode::parse(mode.as_str());
        assert_eq!(
            parsed,
            Some(mode),
            "parse(as_str()) should return the original mode for {:?}",
            mode
        );
    }
}

#[test]
fn node_list_exchange_mode_parse_as_str_roundtrip() {
    for mode in [NodeListExchangeMode::Pull, NodeListExchangeMode::Push] {
        let parsed = NodeListExchangeMode::parse(mode.as_str());
        assert_eq!(
            parsed,
            Some(mode),
            "parse(as_str()) should return the original exchange mode for {:?}",
            mode
        );
    }
}

#[test]
fn spatial_partition_type_parse_as_str_roundtrip() {
    for partition_type in [
        SpatialPartitionType::Fibonacci,
        SpatialPartitionType::Tetrahedron,
        SpatialPartitionType::Cube,
        SpatialPartitionType::Octahedron,
        SpatialPartitionType::Dodecahedron,
        SpatialPartitionType::Icosahedron,
    ] {
        let parsed = SpatialPartitionType::parse(partition_type.as_str());
        assert_eq!(
            parsed,
            Some(partition_type),
            "parse(as_str()) should return the original partition type for {:?}",
            partition_type
        );
    }
}

#[test]
fn config_default_connection_mode_is_node_list_aoi_guard() {
    let config = Config::new_default();
    assert_eq!(
        config.dnve.connection_mode,
        ConnectionMode::NodeListAoiGuard,
        "デフォルト接続モードは NodeListAoiGuard であるべき"
    );
}

#[test]
fn config_default_node_list_exchange_mode_is_pull() {
    let config = Config::new_default();
    assert_eq!(
        config.dnve.node_list_exchange_mode,
        NodeListExchangeMode::Pull,
        "デフォルトの node-list 交換方式は pull であるべき"
    );
}

#[test]
fn config_default_spatial_partition_type_is_dodecahedron() {
    let config = Config::new_default();
    assert_eq!(
        config.dnve.spatial_partition_type,
        SpatialPartitionType::Dodecahedron,
        "デフォルトの空間分割 type は Dodecahedron であるべき"
    );
}

#[test]
fn config_default_flat_values_match_expected_profile() {
    let config = Config::new_default();
    let json = config.to_json_string();
    let flat: serde_json::Value =
        serde_json::from_str(&json).expect("default config should serialize to flat JSON");

    assert_eq!(flat["maxConnectionCount"], 30);
    assert_eq!(flat["connectionBalancerIntervalSeconds"], 2.0);
    assert_eq!(flat["expireSeconds"], 10.0);
    assert_eq!(flat["aoiRange"], 10.0);
    assert_eq!(flat["hopCount"], 2);
    assert_eq!(flat["forceDisconnectCount"], 0);
    assert_eq!(flat["heartbeatIntervalSeconds"], 1.0);
    assert_eq!(flat["nodeListIntervalSeconds"], 2.0);
    assert_eq!(flat["spatialDistanceLayers"], 1);
    assert_eq!(flat["spatialDensityResolution"], 6);
    assert_eq!(flat["spatialDensityEncoding"], "byte");
    assert_eq!(flat["spatialPartitionType"], "dodecahedron");
    assert_eq!(flat["directionThreshold"], 0.0);
    assert_eq!(flat["connectionMode"], "node_list_aoi_guard");
    assert_eq!(flat["nodeListExchangeMode"], "pull");
}

#[test]
fn config_to_json_includes_connection_mode() {
    let mut config = Config::new_default();
    config.dnve.connection_mode = ConnectionMode::NodeListProximity;
    let json = config.to_json_string();
    assert!(
        json.contains("node_list_proximity"),
        "to_json_string は connectionMode フィールドを含むべき: got {json}"
    );
}

#[test]
fn config_to_json_includes_node_list_exchange_mode() {
    let mut config = Config::new_default();
    config.dnve.node_list_exchange_mode = NodeListExchangeMode::Push;
    let json = config.to_json_string();
    assert!(
        json.contains(r#""nodeListExchangeMode":"push""#),
        "to_json_string は nodeListExchangeMode フィールドを含むべき: got {json}"
    );
}

#[test]
fn config_to_json_includes_spatial_partition_type() {
    let mut config = Config::new_default();
    config.dnve.spatial_partition_type = SpatialPartitionType::Icosahedron;
    let json = config.to_json_string();
    assert!(
        json.contains(r#""spatialPartitionType":"icosahedron""#),
        "to_json_string は spatialPartitionType フィールドを含むべき: got {json}"
    );
}

#[test]
fn config_update_from_json_sets_connection_mode() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(r#"{"connectionMode":"node_list_directional"}"#);
    assert!(updated, "update_from_json should return true");
    assert_eq!(
        config.dnve.connection_mode,
        ConnectionMode::NodeListDirectional,
        "update_from_json で接続モードが更新されるべき"
    );
}

#[test]
fn config_update_from_json_sets_node_list_aoi_proximity_mode() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(r#"{"connectionMode":"node_list_aoi_proximity"}"#);
    assert!(updated, "update_from_json should return true");
    assert_eq!(
        config.dnve.connection_mode,
        ConnectionMode::NodeListAoiProximity,
        "update_from_json で node_list_aoi_proximity が設定できるべき"
    );
}

#[test]
fn config_update_from_json_sets_p_sense_mode() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(r#"{"connectionMode":"p_sense"}"#);
    assert!(updated, "update_from_json should return true");
    assert_eq!(
        config.dnve.connection_mode,
        ConnectionMode::PSense,
        "update_from_json で p_sense が設定できるべき"
    );
}

#[test]
fn config_update_from_json_sets_node_list_exchange_mode() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(r#"{"nodeListExchangeMode":"push"}"#);
    assert!(updated, "update_from_json should return true");
    assert_eq!(
        config.dnve.node_list_exchange_mode,
        NodeListExchangeMode::Push,
        "update_from_json で node-list 交換方式が更新されるべき"
    );
}

#[test]
fn config_update_from_json_sets_spatial_partition_type() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(r#"{"spatialPartitionType":"cube"}"#);
    assert!(updated, "update_from_json should return true");
    assert_eq!(
        config.dnve.spatial_partition_type,
        SpatialPartitionType::Cube,
        "update_from_json で空間分割 type が更新されるべき"
    );
}

#[test]
fn config_update_from_json_preserves_fractional_expire_seconds() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(r#"{"expireSeconds":1.5}"#);
    assert!(updated, "update_from_json should return true");
    assert_eq!(
        config.limits.expire_node_seconds, 1.5,
        "expireSeconds は小数秒を切り捨てずに保持するべき"
    );
}

#[test]
fn config_update_from_json_unknown_mode_leaves_mode_unchanged() {
    let mut config = Config::new_default();
    config.update_from_json(r#"{"connectionMode":"does_not_exist"}"#);
    assert_eq!(
        config.dnve.connection_mode,
        ConnectionMode::NodeListAoiGuard,
        "不明なモード文字列は現在の接続モードを変更しないべき"
    );
}

#[test]
fn config_update_from_json_unknown_exchange_mode_leaves_mode_unchanged() {
    let mut config = Config::new_default();
    config.update_from_json(r#"{"nodeListExchangeMode":"does_not_exist"}"#);
    assert_eq!(
        config.dnve.node_list_exchange_mode,
        NodeListExchangeMode::Pull,
        "不明な node-list 交換方式は現在の設定を変更しないべき"
    );
}

#[test]
fn config_update_from_json_unknown_partition_type_leaves_type_unchanged() {
    let mut config = Config::new_default();
    config.update_from_json(r#"{"spatialPartitionType":"does_not_exist"}"#);
    assert_eq!(
        config.dnve.spatial_partition_type,
        SpatialPartitionType::Dodecahedron,
        "不明な分割 type は現在の空間分割 type を変更しないべき"
    );
}
