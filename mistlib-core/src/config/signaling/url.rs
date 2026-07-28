use std::net::IpAddr;

pub(super) fn is_supported_http_url(url: &str) -> bool {
    let url = url.trim().to_ascii_lowercase();
    if url.starts_with("https://") {
        return host_from_http_url(&url).is_some();
    }
    url.starts_with("http://") && is_local_http_url(&url)
}

pub(super) fn is_local_http_url(url: &str) -> bool {
    let url = url.trim().to_ascii_lowercase();
    let Some(host) = host_from_http_url(&url) else {
        return false;
    };
    is_local_host(host)
}

pub(super) fn is_local_relay(relay: &str) -> bool {
    let relay = relay.trim().to_ascii_lowercase();
    let Some(rest) = relay
        .strip_prefix("ws://")
        .or_else(|| relay.strip_prefix("wss://"))
    else {
        return false;
    };
    host_from_authority(rest.split('/').next().unwrap_or(rest)).is_some_and(is_local_host)
}

fn host_from_http_url(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    host_from_authority(rest.split('/').next().unwrap_or(rest))
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

fn is_local_host(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::{is_local_http_url, is_local_relay, is_supported_http_url};

    #[test]
    fn local_relay_detection_uses_the_relay_host() {
        assert!(is_local_relay("ws://127.0.0.1:7777"));
        assert!(is_local_relay("ws://localhost:7777"));
        assert!(is_local_relay("wss://localhost:7777"));
        assert!(is_local_relay("ws://[::1]:7777"));
        assert!(!is_local_relay("wss://relay.example.invalid"));
        assert!(!is_local_relay("ws://localhost.example.invalid"));
        assert!(!is_local_relay("ws://127.0.0.1.example.invalid"));
    }

    #[test]
    fn relay_list_url_detection_uses_the_http_host() {
        assert!(is_supported_http_url(
            "https://data.tik-choco.com/server/relays.json"
        ));
        assert!(is_local_http_url("http://127.0.0.1:8080/relays.json"));
        assert!(is_local_http_url("http://localhost:8080/relays.json"));
        assert!(!is_local_http_url(
            "https://data.tik-choco.com/server/relays.json"
        ));
        assert!(!is_supported_http_url(
            "http://relay.example.invalid/list.json"
        ));
        assert!(!is_supported_http_url("wss://relay.example.invalid"));
        assert!(!is_local_http_url(
            "http://localhost.example.invalid/list.json"
        ));
    }
}
