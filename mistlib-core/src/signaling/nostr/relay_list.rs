pub use crate::config::DEFAULT_NOSTR_RELAY_LIST_URL as DEFAULT_RELAY_LIST_URL;
use crate::error::{MistError, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::net::IpAddr;

const MAX_RELAYS: usize = 64;

pub fn parse_relay_list_json(json: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(json)?;
    let mut relays = Vec::new();
    collect_relays(&value, &mut relays);
    normalize_relays(relays)
}

pub fn normalize_relays(relays: Vec<String>) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for relay in relays {
        let relay = relay.trim().to_string();
        if relay.is_empty() || !is_relay_url(&relay) || !seen.insert(relay.clone()) {
            continue;
        }
        out.push(relay);
        if out.len() >= MAX_RELAYS {
            break;
        }
    }
    if out.is_empty() {
        return Err(MistError::Signaling(
            "Nostr relay list did not contain any ws:// or wss:// relays".to_string(),
        ));
    }
    Ok(out)
}

/// Returns whether a WebSocket relay URL has a loopback host.
pub fn is_local_relay(relay: &str) -> bool {
    let relay = relay.trim().to_ascii_lowercase();
    let Some(rest) = relay
        .strip_prefix("ws://")
        .or_else(|| relay.strip_prefix("wss://"))
    else {
        return false;
    };
    host_from_authority(rest.split('/').next().unwrap_or(rest)).is_some_and(|host| {
        host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    })
}

/// Split already-normalized relay URLs into loopback and other relays.
pub fn split_local_relays(relays: Vec<String>) -> (Vec<String>, Vec<String>) {
    relays.into_iter().partition(|relay| is_local_relay(relay))
}

fn collect_relays(value: &Value, relays: &mut Vec<String>) {
    match value {
        Value::String(url) => relays.push(url.clone()),
        Value::Array(items) => {
            for item in items {
                collect_relays(item, relays);
            }
        }
        Value::Object(map) => {
            for key in ["relays", "servers", "nostrRelays"] {
                if let Some(value) = map.get(key) {
                    collect_relays(value, relays);
                }
            }
            if let Some(Value::Object(nostr)) = map.get("nostr") {
                if let Some(value) = nostr.get("relays") {
                    collect_relays(value, relays);
                }
            }
            if let Some(Value::String(url)) = map.get("url") {
                relays.push(url.clone());
            }
        }
        _ => {}
    }
}

fn is_relay_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
    else {
        return false;
    };
    host_from_authority(rest.split('/').next().unwrap_or(rest)).is_some()
}

fn host_from_authority(authority: &str) -> Option<&str> {
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::{
        is_local_relay, normalize_relays, parse_relay_list_json, split_local_relays,
        DEFAULT_RELAY_LIST_URL,
    };

    #[test]
    fn parses_array_relay_list() {
        let relays = parse_relay_list_json(
            r#"["wss://relay-a.example"," ws://127.0.0.1:7777 ","https://bad"]"#,
        )
        .unwrap();

        assert_eq!(
            relays,
            vec![
                "wss://relay-a.example".to_string(),
                "ws://127.0.0.1:7777".to_string()
            ]
        );
    }

    #[test]
    fn parses_object_relay_list_variants() {
        let relays = parse_relay_list_json(
            r#"{
                "relays": [{"url":"wss://relay-a.example"}, "wss://relay-b.example"],
                "nostr": {"relays": ["wss://relay-c.example"]}
            }"#,
        )
        .unwrap();

        assert_eq!(relays.len(), 3);
        assert!(relays.contains(&"wss://relay-a.example".to_string()));
        assert!(relays.contains(&"wss://relay-c.example".to_string()));
    }

    #[test]
    fn relay_list_deduplicates_and_rejects_empty_result() {
        let relays = normalize_relays(vec![
            "wss://relay-a.example".to_string(),
            "wss://relay-a.example".to_string(),
        ])
        .unwrap();
        assert_eq!(relays, vec!["wss://relay-a.example".to_string()]);

        assert!(parse_relay_list_json(r#"{"relays":["https://not-relay"]}"#).is_err());
        assert!(parse_relay_list_json(r#"{"relays":["wss://","ws://:7777"]}"#).is_err());
    }

    #[test]
    fn detects_only_loopback_relay_hosts() {
        for relay in [
            " ws://LOCALHOST:7777 ",
            "wss://127.42.3.4",
            "ws://[::1]:7777/path",
            "WS://user@localhost/",
        ] {
            assert!(is_local_relay(relay), "{relay}");
        }
        for relay in [
            "http://localhost",
            "ws://localhost.example",
            "ws://127.0.0.1.example",
            "ws://[::2]",
            "ws://128.0.0.1",
            "ws://",
        ] {
            assert!(!is_local_relay(relay), "{relay}");
        }
    }

    #[test]
    fn normalized_relays_are_partitioned_for_initial_phase() {
        let relays = normalize_relays(vec![
            " wss://remote.example ".into(),
            "ws://localhost:7777".into(),
            "ws://localhost:7777".into(),
            "https://localhost".into(),
            "ws://127.10.2.3".into(),
        ])
        .unwrap();
        let (local, remote) = split_local_relays(relays);
        assert_eq!(local, ["ws://localhost:7777", "ws://127.10.2.3"]);
        assert_eq!(remote, ["wss://remote.example"]);
    }

    #[test]
    fn default_relay_list_url_is_the_tik_choco_endpoint() {
        assert_eq!(
            DEFAULT_RELAY_LIST_URL,
            "https://data.tik-choco.com/server/relays.json"
        );
    }
}
