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

    assert_eq!(flat["signalingUrl"], "wss://rtc.tik-choco.com/signaling");
    assert_eq!(flat["signaling"]["mode"], "nostr");
    assert!(
        flat["signaling"]["nostr"]["relays"]
            .as_array()
            .map(|r| r.is_empty())
            .unwrap_or(true),
        "default relays should be empty"
    );
    assert_eq!(flat["signaling"]["nostr"]["discoveryKind"], 25049);
    assert_eq!(flat["signaling"]["nostr"]["messageKind"], 25050);
    assert_eq!(flat["signaling"]["nostr"]["ttlSeconds"], 600);
    assert_eq!(
        flat["signaling"]["nostr"]["inviteSalt"],
        "nostr-sig-test-local-salt"
    );
    assert_eq!(flat["signaling"]["nostr"]["inviteCode"], "dev-invite-001");
    assert_eq!(flat["maxConnectionCount"], 30);
    assert_eq!(flat["connectionBalancerIntervalSeconds"], 2.0);
    assert_eq!(flat["expireSeconds"], 10.0);
    assert_eq!(flat["aoiRange"], 10.0);
    assert_eq!(flat["hopCount"], 2);
    assert_eq!(flat["forceDisconnectCount"], 0);
    assert_eq!(flat["storageMaxCapacityMb"], 8192);
    assert_eq!(flat["heartbeatIntervalSeconds"], 1.0);
    assert_eq!(flat["nodeListIntervalSeconds"], 2.0);
    assert_eq!(flat["pingIntervalSeconds"], 1.0);
    assert_eq!(flat["pingTimeoutCount"], 5);
    assert_eq!(flat["spatialDistanceLayers"], 1);
    assert_eq!(flat["spatialDensityResolution"], 6);
    assert_eq!(flat["spatialDensityEncoding"], "byte");
    assert_eq!(flat["spatialPartitionType"], "dodecahedron");
    assert_eq!(flat["directionThreshold"], 0.0);
    assert_eq!(flat["connectionMode"], "node_list_aoi_guard");
    assert_eq!(flat["nodeListExchangeMode"], "pull");
}

#[test]
fn config_update_from_json_sets_storage_max_capacity_mb() {
    let mut config = Config::new_default();
    let updated = config.update_from_json(r#"{"storageMaxCapacityMb":4096}"#);
    assert!(updated.is_ok(), "update_from_json should return Ok");
    assert_eq!(config.storage.max_capacity_mb, 4096);
}

#[test]
fn config_full_json_without_storage_uses_default_capacity() {
    let json = r#"{
        "signalingUrl": "wss://example.invalid/signaling",
        "limits": {
            "max_connection_count": 30,
            "expire_node_seconds": 10.0,
            "hop_count": 2,
            "reserved_connection_count": 1,
            "force_disconnect_count": 0
        },
        "dnve": {
            "density_max_range": 64.0,
            "distance_layers": 1,
            "density_resolution": 6,
            "density_encoding": "byte",
            "spatial_partition_type": "dodecahedron",
            "direction_threshold": 0.0,
            "aoi_range": 10.0,
            "connection_mode": "node_list_aoi_guard",
            "node_list_exchange_mode": "pull"
        },
        "intervals": {
            "connection_balancer": 2.0,
            "heartbeat": 1.0,
            "node_list": 2.0
        },
        "webrtc": {
            "iceServers": []
        }
    }"#;

    let config: Config = serde_json::from_str(json).expect("old full config should deserialize");
    assert_eq!(config.storage.max_capacity_mb, 8192);
    assert_eq!(
        config.intervals.ping, 1.0,
        "pre-existing full config JSON without `ping` should fall back to the default interval"
    );
    assert_eq!(
        config.limits.ping_timeout_count, 5,
        "pre-existing full config JSON without `ping_timeout_count` should fall back to the default"
    );
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
    assert!(updated.is_ok(), "update_from_json should return Ok");
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
    assert!(updated.is_ok(), "update_from_json should return Ok");
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
    assert!(updated.is_ok(), "update_from_json should return Ok");
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
    assert!(updated.is_ok(), "update_from_json should return Ok");
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
    assert!(updated.is_ok(), "update_from_json should return Ok");
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
    assert!(updated.is_ok(), "update_from_json should return Ok");
    assert_eq!(
        config.limits.expire_node_seconds, 1.5,
        "expireSeconds は小数秒を切り捨てずに保持するべき"
    );
}

#[test]
fn config_update_from_json_unknown_mode_leaves_mode_unchanged() {
    let mut config = Config::new_default();
    let result = config.update_from_json(r#"{"connectionMode":"does_not_exist"}"#);
    assert!(result.is_err(), "不明なモード文字列はエラーになるべき");
    assert_eq!(
        config.dnve.connection_mode,
        ConnectionMode::NodeListAoiGuard,
        "不明なモード文字列は現在の接続モードを変更しないべき"
    );
}

#[test]
fn config_update_from_json_unknown_exchange_mode_leaves_mode_unchanged() {
    let mut config = Config::new_default();
    let result = config.update_from_json(r#"{"nodeListExchangeMode":"does_not_exist"}"#);
    assert!(
        result.is_err(),
        "不明な node-list 交換方式はエラーになるべき"
    );
    assert_eq!(
        config.dnve.node_list_exchange_mode,
        NodeListExchangeMode::Pull,
        "不明な node-list 交換方式は現在の設定を変更しないべき"
    );
}

#[test]
fn config_update_from_json_unknown_partition_type_leaves_type_unchanged() {
    let mut config = Config::new_default();
    let result = config.update_from_json(r#"{"spatialPartitionType":"does_not_exist"}"#);
    assert!(result.is_err(), "不明な分割 type はエラーになるべき");
    assert_eq!(
        config.dnve.spatial_partition_type,
        SpatialPartitionType::Dodecahedron,
        "不明な分割 type は現在の空間分割 type を変更しないべき"
    );
}

#[test]
fn config_update_from_json_unknown_key_is_rejected_with_key_name_in_message() {
    let mut config = Config::new_default();
    // "maxConnectionCount" のtypo。flat JSON としては deny_unknown_fields で拒否される。
    let result = config.update_from_json(r#"{"maxConnectionCont":30}"#);
    let err = result.expect_err("typo キーはエラーになるべき");
    let msg = err.to_string();
    assert!(
        msg.contains("maxConnectionCont"),
        "エラーメッセージに未知キー名を含むべき: {msg}"
    );
}

#[test]
fn config_update_from_json_invalid_enum_value_lists_allowed_values() {
    let mut config = Config::new_default();
    let result = config.update_from_json(r#"{"connectionMode":"does_not_exist"}"#);
    let err = result.expect_err("不正な connectionMode はエラーになるべき");
    let msg = err.to_string();
    assert!(
        msg.contains("connectionMode"),
        "エラーメッセージにキー名を含むべき: {msg}"
    );
    assert!(
        msg.contains("node_list_aoi_guard") && msg.contains("p_sense"),
        "エラーメッセージに許容値一覧を含むべき: {msg}"
    );
}

#[test]
fn config_update_from_json_invalid_spatial_partition_type_lists_allowed_values() {
    let mut config = Config::new_default();
    let result = config.update_from_json(r#"{"spatialPartitionType":"does_not_exist"}"#);
    let err = result.expect_err("不正な spatialPartitionType はエラーになるべき");
    let msg = err.to_string();
    assert!(msg.contains("spatialPartitionType"), "got: {msg}");
    assert!(
        msg.contains("dodecahedron") && msg.contains("icosahedron"),
        "got: {msg}"
    );
}

#[test]
fn config_default_max_message_bytes_is_65536() {
    let config = Config::new_default();
    assert_eq!(
        config.limits.max_message_bytes, 65536,
        "max_message_bytes のデフォルトは 64KiB であるべき"
    );
}

#[test]
fn config_update_from_json_sets_max_message_bytes() {
    let mut config = Config::new_default();
    let result = config.update_from_json(r#"{"maxMessageBytes":131072}"#);
    assert!(result.is_ok(), "update_from_json should return Ok");
    assert_eq!(config.limits.max_message_bytes, 131072);
}

#[test]
fn config_update_from_json_applies_full_config_json() {
    let mut config = Config::new_default();
    let json = r#"{
        "signalingUrl": "wss://example.invalid/signaling",
        "limits": {
            "max_connection_count": 12,
            "expire_node_seconds": 10.0,
            "hop_count": 2,
            "reserved_connection_count": 1,
            "force_disconnect_count": 0
        },
        "dnve": {
            "density_max_range": 64.0,
            "distance_layers": 1,
            "density_resolution": 6,
            "density_encoding": "byte",
            "spatial_partition_type": "dodecahedron",
            "direction_threshold": 0.0,
            "aoi_range": 10.0,
            "connection_mode": "node_list_aoi_guard",
            "node_list_exchange_mode": "pull"
        },
        "intervals": {
            "connection_balancer": 2.0,
            "heartbeat": 1.0,
            "node_list": 2.0
        },
        "webrtc": {
            "iceServers": []
        }
    }"#;

    let result = config.update_from_json(json);
    assert!(result.is_ok(), "フル Config JSON の適用は成功するべき");
    assert_eq!(config.limits.max_connection_count, 12);
    assert_eq!(config.signaling_url, "wss://example.invalid/signaling");
}
