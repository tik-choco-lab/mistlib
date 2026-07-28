use super::*;
use mistlib_core::config::IceServer;

#[test]
fn empty_config_maps_to_empty_vec() {
    let mapped = map_ice_servers(&[]);
    assert!(
        mapped.is_empty(),
        "a user-configured empty ice_servers list must be honored as empty, not fall back to a default"
    );
}

#[test]
fn maps_urls_username_and_credential() {
    let configured = vec![IceServer {
        urls: vec!["turn:turn.example.com:3478".to_string()],
        username: Some("alice".to_string()),
        credential: Some("secret".to_string()),
    }];

    let mapped = map_ice_servers(&configured);

    assert_eq!(mapped.len(), 1);
    assert_eq!(
        mapped[0].urls,
        vec!["turn:turn.example.com:3478".to_string()]
    );
    assert_eq!(mapped[0].username, "alice");
    assert_eq!(mapped[0].credential, "secret");
}

#[test]
fn missing_username_and_credential_map_to_empty_strings() {
    let configured = vec![IceServer {
        urls: vec!["stun:stun.example.com:19302".to_string()],
        username: None,
        credential: None,
    }];

    let mapped = map_ice_servers(&configured);

    assert_eq!(mapped.len(), 1);
    assert_eq!(mapped[0].username, "");
    assert_eq!(mapped[0].credential, "");
}

#[test]
fn credential_less_turn_entry_is_dropped() {
    // webrtc-rs rejects turn/turns without credentials inside
    // new_peer_connection; forwarding such an entry would fail every
    // create_pc for the whole session, so it must be filtered out here.
    let configured = vec![
        IceServer {
            urls: vec!["turn:turn.example.com:3478".to_string()],
            username: None,
            credential: None,
        },
        IceServer {
            urls: vec!["stun:stun.example.com:19302".to_string()],
            username: None,
            credential: None,
        },
    ];

    let mapped = map_ice_servers(&configured);

    assert_eq!(mapped.len(), 1);
    assert_eq!(
        mapped[0].urls,
        vec!["stun:stun.example.com:19302".to_string()]
    );
}

#[test]
fn entry_without_urls_is_dropped() {
    let configured = vec![IceServer {
        urls: vec![],
        username: None,
        credential: None,
    }];

    assert!(map_ice_servers(&configured).is_empty());
}

#[test]
fn multiple_servers_preserve_order() {
    let configured = vec![
        IceServer {
            urls: vec!["stun:a.example.com".to_string()],
            username: None,
            credential: None,
        },
        IceServer {
            urls: vec!["turn:b.example.com".to_string()],
            username: Some("u".to_string()),
            credential: Some("p".to_string()),
        },
    ];

    let mapped = map_ice_servers(&configured);

    assert_eq!(mapped.len(), 2);
    assert_eq!(mapped[0].urls, vec!["stun:a.example.com".to_string()]);
    assert_eq!(mapped[1].urls, vec!["turn:b.example.com".to_string()]);
}
