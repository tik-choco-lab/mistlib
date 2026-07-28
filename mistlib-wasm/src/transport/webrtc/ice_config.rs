use mistlib_core::config::IceServer;

/// A single ICE server, filtered and ready to render into a JS `RTCIceServer`
/// object. Kept as a plain Rust type (rather than building
/// `web_sys::RtcIceServer` directly from `mistlib_core::config::IceServer`)
/// so the filtering rule below is testable on the host, without a
/// browser/wasm32 runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceServerPlan {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

/// Converts config `IceServer` entries into render-ready plans. Drops
/// unusable entries (no URLs, or turn/turns without credentials -- see
/// `IceServer::is_usable`) with a warning: browsers throw
/// `InvalidAccessError` at `RTCPeerConnection` construction for a
/// credential-less TURN entry, which would fail every connection attempt in
/// the session. Everything else passes through unchanged, so an
/// intentionally empty `servers` list (no STUN/TURN) stays empty rather than
/// falling back to a default.
pub fn build_ice_server_plans(servers: &[IceServer]) -> Vec<IceServerPlan> {
    servers
        .iter()
        .filter(|server| {
            if server.is_usable() {
                true
            } else {
                tracing::warn!(
                    "ignoring unusable ICE server entry {:?}: turn/turns URLs require a \
                     non-empty username and credential",
                    server.urls
                );
                false
            }
        })
        .map(|server| IceServerPlan {
            urls: server.urls.clone(),
            username: server.username.clone(),
            credential: server.credential.clone(),
        })
        .collect()
}

/// Renders `plans` into the `iceServers` array expected by
/// `RtcConfiguration::set_ice_servers`. `username`/`credential` are only set
/// when present, matching how the browser distinguishes anonymous STUN
/// servers from authenticated TURN servers.
///
/// wasm32-only: the crate (`lib.rs`) only compiles on `wasm32` in the first
/// place, and `mistlib-wasm/tests/ice_config.rs` pulls this file in directly
/// on the host to unit-test `build_ice_server_plans` above -- gating this
/// function keeps that host build free of a js-sys/web-sys dependency it
/// can't exercise anyway.
#[cfg(target_arch = "wasm32")]
pub fn ice_server_plans_to_js(plans: &[IceServerPlan]) -> js_sys::Array {
    let array = js_sys::Array::new();
    for plan in plans {
        let ice_server = web_sys::RtcIceServer::new();
        let urls = js_sys::Array::new();
        for url in &plan.urls {
            urls.push(&wasm_bindgen::JsValue::from_str(url));
        }
        ice_server.set_urls(&urls);
        if let Some(username) = &plan.username {
            ice_server.set_username(username);
        }
        if let Some(credential) = &plan.credential {
            ice_server.set_credential(credential);
        }
        array.push(&ice_server);
    }
    array
}
