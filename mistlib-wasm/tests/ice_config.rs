#[path = "../src/transport/webrtc/ice_config.rs"]
mod ice_config;

use ice_config::{build_ice_server_plans, IceServerPlan};
use mistlib_core::config::IceServer;

fn ice_server(urls: &[&str], username: Option<&str>, credential: Option<&str>) -> IceServer {
    IceServer {
        urls: urls.iter().map(|s| s.to_string()).collect(),
        username: username.map(String::from),
        credential: credential.map(String::from),
    }
}

#[test]
fn preserves_urls_credentials_and_input_order() {
    let servers = [
        ice_server(&["stun:a.example.com", "stun:b.example.com"], None, None),
        ice_server(
            &["turn:turn.example.com:3478"],
            Some("alice"),
            Some("s3cr3t"),
        ),
    ];

    assert_eq!(
        build_ice_server_plans(&servers),
        vec![
            IceServerPlan {
                urls: vec![
                    "stun:a.example.com".to_string(),
                    "stun:b.example.com".to_string(),
                ],
                username: None,
                credential: None,
            },
            IceServerPlan {
                urls: vec!["turn:turn.example.com:3478".to_string()],
                username: Some("alice".to_string()),
                credential: Some("s3cr3t".to_string()),
            },
        ]
    );
}

#[test]
fn filters_invalid_entries_without_discarding_valid_servers() {
    // Browsers reject credential-less TURN entries at PeerConnection
    // construction, so one invalid entry must not poison the whole config.
    let servers = [
        ice_server(&[], None, None),
        ice_server(&["turn:turn.example.com:3478"], None, None),
        ice_server(&["stun:stun.example.com:19302"], None, None),
        ice_server(&["turn:valid.example.com"], Some("u"), Some("p")),
    ];

    assert_eq!(
        build_ice_server_plans(&servers),
        vec![
            IceServerPlan {
                urls: vec!["stun:stun.example.com:19302".to_string()],
                username: None,
                credential: None,
            },
            IceServerPlan {
                urls: vec!["turn:valid.example.com".to_string()],
                username: Some("u".to_string()),
                credential: Some("p".to_string()),
            },
        ]
    );
}
