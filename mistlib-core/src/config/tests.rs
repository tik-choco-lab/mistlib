use super::{default_ice_servers, Config, IceServer, SpatialPartitionType, DEFAULT_STUN_URLS};

fn ice_server(urls: &[&str], username: Option<&str>, credential: Option<&str>) -> IceServer {
    IceServer {
        urls: urls.iter().map(|s| s.to_string()).collect(),
        username: username.map(String::from),
        credential: credential.map(String::from),
    }
}

#[test]
fn stun_server_without_credentials_is_usable() {
    assert!(ice_server(&["stun:stun.example.com:19302"], None, None).is_usable());
}

#[test]
fn turn_server_with_credentials_is_usable() {
    assert!(ice_server(&["turn:turn.example.com:3478"], Some("u"), Some("p")).is_usable());
}

#[test]
fn turn_server_without_credentials_is_not_usable() {
    // webrtc-rs and browsers both reject this at PeerConnection construction,
    // which would fail every connection attempt in the session.
    assert!(!ice_server(&["turn:turn.example.com:3478"], None, None).is_usable());
    assert!(!ice_server(&["turn:turn.example.com:3478"], Some("u"), None).is_usable());
    assert!(!ice_server(&["turn:turn.example.com:3478"], None, Some("p")).is_usable());
    assert!(!ice_server(&["turn:turn.example.com:3478"], Some(""), Some("")).is_usable());
}

#[test]
fn turns_scheme_is_detected_case_insensitively() {
    assert!(!ice_server(&["TURNS:turn.example.com:5349"], None, None).is_usable());
    assert!(ice_server(&["TURNS:turn.example.com:5349"], Some("u"), Some("p")).is_usable());
}

#[test]
fn mixed_stun_and_turn_urls_require_credentials() {
    // One credential-less turn URL poisons the whole entry (per-URL
    // validation in the PC constructor), even if a stun URL is also present.
    assert!(!ice_server(
        &["stun:stun.example.com:19302", "turn:turn.example.com:3478"],
        None,
        None
    )
    .is_usable());
}

#[test]
fn entry_without_urls_is_not_usable() {
    assert!(!ice_server(&[], None, None).is_usable());
}

#[test]
fn auto_direction_threshold_is_tighter_than_legacy_default_for_26_dirs() {
    let threshold = Config::effective_direction_threshold_for(26, 0.0);
    assert!(
        threshold > 0.7,
        "auto threshold should be tighter than 0.7, got {threshold}"
    );
    assert!(
        threshold < 1.0,
        "auto threshold must remain a valid cosine, got {threshold}"
    );
}

#[test]
fn explicit_direction_threshold_override_is_preserved() {
    let threshold = Config::effective_direction_threshold_for(26, 0.82);
    assert!((threshold - 0.82).abs() < f32::EPSILON);
}

#[test]
fn auto_direction_threshold_uses_partition_direction_count() {
    let mut config = Config::new_default();
    config.dnve.spatial_partition_type = SpatialPartitionType::Icosahedron;
    config.dnve.density_resolution = 6;

    assert_eq!(
        config.effective_direction_threshold(),
        Config::effective_direction_threshold_for(20, 0.0)
    );
}

/// `webrtc.ice_servers` used to be reachable only by supplying a *complete*
/// nested `Config` -- every section spelled out -- because `FlatConfig` had no
/// field for it and `deny_unknown_fields` rejected one. That left a host
/// application no practical way to point mistlib at its own TURN relay, which
/// is the only thing that gets a connection through symmetric NAT.
#[test]
fn flat_config_can_set_stun_and_turn_servers() {
    let mut config = Config::new_default();
    config
        .update_from_json(
            r#"{
                "aoiRange": 20.0,
                "iceServers": [
                    { "urls": ["stun:stun.example.com:3478"] },
                    {
                        "urls": ["turn:turn.example.com:3478"],
                        "username": "u",
                        "credential": "p"
                    }
                ]
            }"#,
        )
        .expect("flat config carrying iceServers must parse");

    assert_eq!(config.dnve.aoi_range, 20.0);
    assert_eq!(config.webrtc.ice_servers.len(), 2);
    let turn = &config.webrtc.ice_servers[1];
    assert_eq!(turn.urls, vec!["turn:turn.example.com:3478"]);
    assert_eq!(turn.username.as_deref(), Some("u"));
    assert!(turn.is_usable(), "TURN with credentials must be usable");
}

/// `get_config()` emits the flat form, so whatever it prints has to be valid
/// input to `set_config()` -- otherwise a read-modify-write round trip would
/// silently drop the host's TURN credentials.
#[test]
fn ice_servers_survive_a_flat_json_round_trip() {
    let mut config = Config::new_default();
    config
        .update_from_json(
            r#"{"iceServers":[{"urls":["turn:turn.example.com:3478"],"username":"u","credential":"p"}]}"#,
        )
        .unwrap();

    let dumped = config.to_json_string();
    let mut reloaded = Config::new_default();
    reloaded
        .update_from_json(&dumped)
        .expect("get_config() output must be accepted by set_config()");

    assert_eq!(reloaded.webrtc.ice_servers, config.webrtc.ice_servers);
}

/// Every default STUN entry must be a bare `stun:` URL: a `turn:` default would
/// need credentials, and `IceServer::is_usable` would drop the whole entry at
/// construction time, leaving peers with no reflexive candidate at all.
#[test]
fn default_ice_servers_are_usable_stun_only() {
    let servers = default_ice_servers();
    assert_eq!(servers.len(), 1);
    assert!(servers[0].is_usable());
    assert_eq!(servers[0].urls.len(), DEFAULT_STUN_URLS.len());
    for url in &servers[0].urls {
        assert!(url.starts_with("stun:"), "unexpected default ICE URL: {url}");
    }
}
