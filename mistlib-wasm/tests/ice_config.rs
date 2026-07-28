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
fn passes_through_a_plain_stun_server() {
    let servers = [ice_server(&["stun:stun.l.google.com:19302"], None, None)];

    let plans = build_ice_server_plans(&servers);

    assert_eq!(
        plans,
        vec![IceServerPlan {
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            username: None,
            credential: None,
        }]
    );
}

#[test]
fn keeps_turn_credentials() {
    let servers = [ice_server(
        &["turn:turn.example.com:3478"],
        Some("alice"),
        Some("s3cr3t"),
    )];

    let plans = build_ice_server_plans(&servers);

    assert_eq!(plans[0].username.as_deref(), Some("alice"));
    assert_eq!(plans[0].credential.as_deref(), Some("s3cr3t"));
}

#[test]
fn drops_entries_with_no_urls() {
    let servers = [ice_server(&[], None, None)];

    assert!(build_ice_server_plans(&servers).is_empty());
}

#[test]
fn respects_an_explicitly_empty_list() {
    assert!(build_ice_server_plans(&[]).is_empty());
}

#[test]
fn drops_credential_less_turn_entry() {
    // Browsers throw InvalidAccessError at RTCPeerConnection construction
    // for a turn/turns URL without credentials, which would fail every
    // connection attempt in the session.
    let servers = [
        ice_server(&["turn:turn.example.com:3478"], None, None),
        ice_server(&["stun:stun.example.com:19302"], None, None),
    ];

    let plans = build_ice_server_plans(&servers);

    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].urls,
        vec!["stun:stun.example.com:19302".to_string()]
    );
}

#[test]
fn preserves_multiple_urls_on_one_server() {
    let servers = [ice_server(
        &["stun:a.example.com", "stun:b.example.com"],
        None,
        None,
    )];

    let plans = build_ice_server_plans(&servers);

    assert_eq!(plans[0].urls.len(), 2);
}

#[test]
fn drops_only_the_empty_entry_among_several() {
    let servers = [
        ice_server(&["stun:a.example.com"], None, None),
        ice_server(&[], None, None),
        ice_server(&["turn:b.example.com"], Some("u"), Some("p")),
    ];

    let plans = build_ice_server_plans(&servers);

    assert_eq!(plans.len(), 2);
}
