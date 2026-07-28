use bytes::Bytes;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::mpsc;
use tracing_subscriber::{prelude::*, EnvFilter};

use crate::engine::*;
use mistlib_core::action::OverlayAction;
use mistlib_core::config::Config;
use mistlib_core::layers::L0Engine;
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

pub use mistlib_core::types::{
    DELIVERY_RELIABLE, DELIVERY_UNRELIABLE, DELIVERY_UNRELIABLE_ORDERED,
};

#[derive(Clone)]
struct SendRequest {
    /// `None` = roomless (auto-routed per SPEC-15 rule 5/broadcast to all
    /// sessions); `Some(room_id)` = scoped to exactly that room's session.
    room: Option<String>,
    target_node: NodeId,
    bytes: Bytes,
    delivery: DeliveryMethod,
}

#[derive(Clone, Copy)]
struct PositionUpdate {
    x: f32,
    y: f32,
    z: f32,
}

static SEND_QUEUE: LazyLock<Mutex<Option<mpsc::Sender<SendRequest>>>> =
    LazyLock::new(|| Mutex::new(None));

static STATS_CACHE: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new("{}".to_string()));
static STATS_WORKER_STARTED: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));
/// Roomless `update_position`: applies to every active session (SPEC-15 rule 6).
static POSITION_CACHE: LazyLock<Mutex<Option<PositionUpdate>>> = LazyLock::new(|| Mutex::new(None));
/// `update_position_in_room`: only the named room's session. Both caches are
/// drained by the same `position_worker` on the same interval.
static POSITION_CACHE_BY_ROOM: LazyLock<Mutex<HashMap<String, PositionUpdate>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static POSITION_WORKER_STARTED: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));

pub static INIT_LOG: LazyLock<()> = LazyLock::new(|| {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("off"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(ExternalLogLayer)
        .try_init();
});

pub fn join_room(room_id: String) {
    ENGINE.l0.join_room(room_id);
}

pub fn init_and_join(id: String, signaling_url: String, room_id: String) {
    init(id, signaling_url);
    join_room(room_id);
}

pub fn register_log_callback(cb: LogCallback) {
    let mut callback = ENGINE.log_callback.lock().unwrap();
    *callback = Some(cb);
}

pub fn register_event_callback(cb: EventCallback) {
    let mut callback = ENGINE.global_callback.lock().unwrap();
    *callback = Some(cb);
}

/// v2 callback: same events as `register_event_callback`, tagged with the
/// room_id they occurred in. Both fire if both are registered (SPEC-15 rule 7).
pub fn register_event_callback_v2(cb: EventCallbackV2) {
    let mut callback = ENGINE.global_callback_v2.lock().unwrap();
    *callback = Some(cb);
}

pub fn register_raw_handler<F>(handler: F)
where
    F: Fn(u32, String, Vec<u8>) + Send + Sync + 'static,
{
    let mut callback = ENGINE.rust_event_callback.lock().unwrap();
    *callback = Some(Arc::new(handler));
}

pub fn clear_raw_handler() {
    let mut callback = ENGINE.rust_event_callback.lock().unwrap();
    *callback = None;
}

/// Register a receiver for remote WebRTC media tracks (audio/video) arriving
/// from peers, across all rooms. Session transports created afterward (each
/// `join_room*` makes one) inherit the handler at construction; sessions
/// already running are wired immediately -- but only their peers connected
/// *after* this call get the per-peer `on_track` hookup, so register before
/// the peers expected to carry media appear.
///
/// Like the other sync app functions this blocks on the engine runtime and
/// must not be called from a tokio worker thread.
pub fn register_media_track_handler(
    tx: mpsc::UnboundedSender<crate::transports::webrtc::MediaTrackEvent>,
) -> crate::error::Result<()> {
    ENGINE
        .runtime
        .block_on(register_media_track_handler_async(tx))
}

/// Async variant of [`register_media_track_handler`]; safe to await anywhere.
pub async fn register_media_track_handler_async(
    tx: mpsc::UnboundedSender<crate::transports::webrtc::MediaTrackEvent>,
) -> crate::error::Result<()> {
    *crate::transports::webrtc::GLOBAL_MEDIA_TX.lock().unwrap() = Some(tx.clone());
    for (_, ctx) in ENGINE.sessions_snapshot().await {
        if let Some(transport) = ctx.webrtc_transport.as_ref() {
            transport.set_media_track_handler(tx.clone());
        }
    }
    Ok(())
}

/// Publishes a local media track into `room_id`'s room: every peer already
/// connected there gets it attached (with renegotiation), and every peer
/// that joins the room afterward gets it automatically -- no further action
/// needed as new peers arrive. This is the cascade/SFU building block: a
/// native app (e.g. mistl) that received a track from one peer -- such as a
/// VRChat screen share relayed over tc-chat -- can re-publish it here so
/// every other peer in the room receives it too, without each of them
/// needing a direct connection back to the original source. Mirrors
/// `mistlib-wasm`'s `publish_local_track` app-level API
/// (`mistlib-wasm/src/app/media.rs`), scoped to a single room instead of
/// fanning out over every joined room (native sessions are already
/// independent `WebRtcTransport`s per room, so there is no roomless-track
/// concept to preserve here).
///
/// Errors if `room_id` isn't currently joined, or if its session has no
/// WebRTC transport (e.g. a signaling-only session).
pub async fn publish_local_track(
    room_id: &str,
    track: Arc<TrackLocalStaticRTP>,
) -> crate::error::Result<()> {
    let transport = resolve_room_webrtc_transport(room_id).await?;
    transport.publish_local_track(track).await
}

/// Reverses [`publish_local_track`] for `room_id`. See its docs for the
/// cascade/SFU use case this pair of functions supports.
pub async fn unpublish_local_track(
    room_id: &str,
    track: Arc<TrackLocalStaticRTP>,
) -> crate::error::Result<()> {
    let transport = resolve_room_webrtc_transport(room_id).await?;
    transport.unpublish_local_track(track).await
}

async fn resolve_room_webrtc_transport(
    room_id: &str,
) -> crate::error::Result<Arc<crate::transports::webrtc::WebRtcTransport>> {
    let ctx = ENGINE.get_session(room_id).await.ok_or_else(|| {
        crate::error::MistError::Internal(format!("room '{room_id}' is not joined"))
    })?;
    ctx.webrtc_transport.clone().ok_or_else(|| {
        crate::error::MistError::Internal(format!("room '{room_id}' has no WebRTC transport"))
    })
}

pub fn init(id: String, signaling_url: String) {
    *INIT_LOG;
    tracing::info!("mistlib::init called");

    let local_id = NodeId(id);
    ENGINE.l0.initialize(local_id, signaling_url);
    ensure_send_worker();
    ensure_stats_worker();
}

fn config_from_json(data: &str) -> Option<Config> {
    let mut config = if let Ok(config) = serde_json::from_str::<Config>(data) {
        config
    } else {
        let mut config = ENGINE.l0.get_config();
        if let Err(err) = config.update_from_json(data) {
            tracing::error!("config_from_json: update_from_json failed: {err}");
            return None;
        }
        config
    };

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
        if let Some(url) = value.get("signalingUrl").and_then(|v| v.as_str()) {
            config.signaling_url = url.to_string();
        }
    }

    config.validate_signaling().then_some(config)
}

pub fn init_with_config(id: String, data: &[u8]) -> bool {
    *INIT_LOG;
    tracing::info!("mistlib::init_with_config called");

    let Ok(json_str) = std::str::from_utf8(data) else {
        tracing::warn!("init_with_config ignored non-UTF-8 config JSON");
        return false;
    };
    let Some(config) = config_from_json(json_str) else {
        tracing::warn!("init_with_config ignored invalid config JSON");
        return false;
    };
    let signaling_url = config.signaling_url.clone();

    ENGINE.l0.set_config(config);
    ENGINE.l0.initialize(NodeId(id), signaling_url);
    ensure_send_worker();
    ensure_stats_worker();
    true
}

pub fn leave_room() {
    ENGINE.l0.leave_room();
}

/// Leaves only `room_id`, leaving every other active room untouched.
pub fn leave_room_id(room_id: String) {
    ENGINE.l0.leave_room_id(room_id);
}

pub fn shutdown() {
    leave_room();
}

pub fn update_position(x: f32, y: f32, z: f32) {
    {
        let mut cache = POSITION_CACHE.lock().unwrap();
        *cache = Some(PositionUpdate { x, y, z });
    }

    ensure_position_worker();
}

/// Room-scoped `update_position`: only `room_id`'s session sees it.
pub fn update_position_in_room(room_id: String, x: f32, y: f32, z: f32) {
    {
        let mut cache = POSITION_CACHE_BY_ROOM.lock().unwrap();
        cache.insert(room_id, PositionUpdate { x, y, z });
    }

    ensure_position_worker();
}

pub fn on_connected(node_id: NodeId) {
    crate::events::on_connected_primary(node_id);
}

pub fn on_disconnected(node_id: NodeId) {
    crate::events::on_disconnected_primary(node_id);
}

pub fn set_config(data: &[u8]) {
    if let Ok(json_str) = std::str::from_utf8(data) {
        if let Some(config) = config_from_json(json_str) {
            ENGINE.l0.set_config(config);
        }
    }
}

pub fn send_message(target_id: String, data: &[u8], method: u32) {
    if let Err(err) = try_send_message(target_id, data, method) {
        tracing::warn!("send_message dropped: {}", err);
    }
}

pub fn try_send_message(target_id: String, data: &[u8], method: u32) -> crate::error::Result<()> {
    enqueue_send(None, target_id, data, method)
}

/// Room-scoped `send_message`: errors (rather than silently falling back) if
/// `room_id` isn't currently joined, since -- unlike the roomless variant --
/// there's no reasonable auto-route to fall back to.
pub fn send_message_in_room(room_id: String, target_id: String, data: &[u8], method: u32) {
    if let Err(err) = try_send_message_in_room(room_id, target_id, data, method) {
        tracing::warn!("send_message_in_room dropped: {}", err);
    }
}

pub fn try_send_message_in_room(
    room_id: String,
    target_id: String,
    data: &[u8],
    method: u32,
) -> crate::error::Result<()> {
    enqueue_send(Some(room_id), target_id, data, method)
}

fn enqueue_send(
    room: Option<String>,
    target_id: String,
    data: &[u8],
    method: u32,
) -> crate::error::Result<()> {
    let Some(sender) = SEND_QUEUE.lock().unwrap().clone() else {
        return Err(crate::error::MistError::Internal(
            "send worker is not initialized".to_string(),
        ));
    };

    sender
        .try_send(SendRequest {
            room,
            target_node: NodeId(target_id),
            bytes: Bytes::copy_from_slice(data),
            delivery: delivery_method(method),
        })
        .map_err(|err| crate::error::MistError::Internal(err.to_string()))
}

pub async fn send_message_direct(
    target_id: String,
    data: Vec<u8>,
    method: u32,
) -> crate::error::Result<()> {
    let target_node = NodeId(target_id);
    let Some(ctx) = ENGINE.resolve_unicast_session(&target_node).await else {
        return Err(crate::error::MistError::Internal(
            "no active session".to_string(),
        ));
    };
    let Some(overlay) = ctx.overlay.as_ref() else {
        return Err(crate::error::MistError::Internal(
            "overlay router is not available".to_string(),
        ));
    };
    let Some(transport) = ctx.webrtc_transport.as_ref() else {
        return Err(crate::error::MistError::Internal(
            "WebRTC transport is not available".to_string(),
        ));
    };

    let action = overlay.wrap_data(&target_node, Bytes::from(data), delivery_method(method));
    let OverlayAction::SendMessage { to, data, method } = action else {
        return Err(crate::error::MistError::Internal(
            "overlay did not produce a send action".to_string(),
        ));
    };

    if to.is_broadcast() {
        transport.broadcast(data, method).await?;
    } else {
        transport.send(&to, data, method).await?;
    }
    Ok(())
}

fn ensure_send_worker() {
    let mut queue_lock = SEND_QUEUE.lock().unwrap();
    if queue_lock.is_some() {
        return;
    }

    let (tx, rx) = mpsc::channel::<SendRequest>(8192);
    *queue_lock = Some(tx);

    ENGINE.runtime.spawn(async move {
        send_worker(rx).await;
    });
}

fn ensure_stats_worker() {
    let mut started = STATS_WORKER_STARTED.lock().unwrap();
    if *started {
        return;
    }
    *started = true;

    ENGINE.runtime.spawn(async move {
        stats_worker().await;
    });
}

fn ensure_position_worker() {
    let mut started = POSITION_WORKER_STARTED.lock().unwrap();
    if *started {
        return;
    }
    *started = true;

    ENGINE.runtime.spawn(async move {
        position_worker().await;
    });
}

fn delivery_method(method: u32) -> DeliveryMethod {
    DeliveryMethod::from_u32(method)
}

/// Union of connected nodes across every active session, deduplicated. A
/// node connected in more than one room only appears once.
pub fn get_connected_nodes() -> Vec<String> {
    ENGINE.runtime.block_on(get_connected_nodes_async())
}

pub async fn get_connected_nodes_async() -> Vec<String> {
    let mut nodes = std::collections::HashSet::new();
    for (_, ctx) in ENGINE.sessions_snapshot().await {
        let session_nodes = ctx
            .webrtc_transport
            .as_ref()
            .map(|transport| transport.get_connected_nodes())
            .unwrap_or_else(|| ctx.transport.get_connected_nodes());
        nodes.extend(session_nodes.into_iter().map(|node| node.0));
    }
    nodes.into_iter().collect()
}

/// Room-scoped counterpart of `get_connected_nodes`: instead of a
/// cross-session union, returns each active session's own room id paired
/// with its connected peers and their per-room connection state. A node
/// connected in more than one room appears once under each room; every
/// active session appears even if it currently has no peers. Added so
/// consumers (mistl's topology view) can show which peer is connected in
/// which room.
pub fn get_room_connections() -> Vec<(String, Vec<(String, String)>)> {
    ENGINE.runtime.block_on(get_room_connections_async())
}

pub async fn get_room_connections_async() -> Vec<(String, Vec<(String, String)>)> {
    let mut rooms = Vec::new();
    for (room_id, ctx) in ENGINE.sessions_snapshot().await {
        let session_nodes = ctx
            .webrtc_transport
            .as_ref()
            .map(|transport| transport.get_connected_nodes())
            .unwrap_or_else(|| ctx.transport.get_connected_nodes());
        let peers = session_nodes
            .into_iter()
            .map(|node| {
                let state = ctx
                    .webrtc_transport
                    .as_ref()
                    .map(|transport| transport.get_connection_state(&node))
                    .unwrap_or_else(|| ctx.transport.get_connection_state(&node));
                (node.0, state.to_string())
            })
            .collect();
        rooms.push((room_id, peers));
    }
    rooms
}

pub fn get_connection_state(node_id: &str) -> String {
    get_connection_state_value(node_id).to_string()
}

pub async fn get_connection_state_async(node_id: &str) -> String {
    get_connection_state_value_async(node_id).await.to_string()
}

pub fn get_connection_state_value(node_id: &str) -> ConnectionState {
    ENGINE
        .runtime
        .block_on(get_connection_state_value_async(node_id))
}

/// The most-connected state seen for `node_id` across every active session
/// (join order): the first non-`Disconnected` state wins, else `Disconnected`.
pub async fn get_connection_state_value_async(node_id: &str) -> ConnectionState {
    let node = NodeId(node_id.to_string());
    for (_, ctx) in ENGINE.sessions_snapshot().await {
        let state = ctx
            .webrtc_transport
            .as_ref()
            .map(|transport| transport.get_connection_state(&node))
            .unwrap_or_else(|| ctx.transport.get_connection_state(&node));
        if state != ConnectionState::Disconnected {
            return state;
        }
    }
    ConnectionState::Disconnected
}

async fn send_worker(mut rx: mpsc::Receiver<SendRequest>) {
    while let Some(req) = rx.recv().await {
        match req.room {
            Some(room_id) => send_in_room(&room_id, req.target_node, req.bytes, req.delivery).await,
            None => send_roomless(req.target_node, req.bytes, req.delivery).await,
        }
    }
}

async fn send_in_room(room_id: &str, target: NodeId, bytes: Bytes, delivery: DeliveryMethod) {
    let Some(ctx) = ENGINE.get_session(room_id).await else {
        tracing::warn!("send_message_in_room dropped: room '{room_id}' is not joined");
        return;
    };
    let Some(l1) = ctx.l1_transport.as_ref() else {
        return;
    };
    if target.0.is_empty() {
        if let Err(err) = l1.broadcast(bytes, delivery).await {
            tracing::warn!("send_message_in_room broadcast dropped in room '{room_id}': {err}");
        }
    } else if let Err(err) = l1.send_message(&target, bytes, delivery).await {
        tracing::warn!(
            "send_message_in_room dropped in room '{room_id}' to {}: {err}",
            target.0
        );
    }
}

async fn send_roomless(target: NodeId, bytes: Bytes, delivery: DeliveryMethod) {
    if target.0.is_empty() {
        for (room_id, ctx) in ENGINE.sessions_snapshot().await {
            if let Some(l1) = ctx.l1_transport.as_ref() {
                if let Err(err) = l1.broadcast(bytes.clone(), delivery).await {
                    tracing::warn!("send_message broadcast dropped in room '{room_id}': {err}");
                }
            }
        }
        return;
    }

    let Some(ctx) = ENGINE.resolve_unicast_session(&target).await else {
        return;
    };
    if let Some(l1) = ctx.l1_transport.as_ref() {
        if let Err(err) = l1.send_message(&target, bytes, delivery).await {
            tracing::warn!("send_message dropped to {}: {err}", target.0);
        }
    }
}

async fn stats_worker() {
    // Prime the cache immediately so the first caller doesn't see stale data.
    update_stats_cache().await;

    let mut interval = tokio::time::interval(web_time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        update_stats_cache().await;
    }
}

async fn position_worker() {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(50));

    loop {
        interval.tick().await;

        let roomless_update = POSITION_CACHE.lock().unwrap().take();
        let per_room_updates: Vec<(String, PositionUpdate)> = {
            let mut cache = POSITION_CACHE_BY_ROOM.lock().unwrap();
            cache.drain().collect()
        };

        if roomless_update.is_none() && per_room_updates.is_empty() {
            continue;
        }

        let sessions = ENGINE.sessions_snapshot().await;

        if let Some(update) = roomless_update {
            for (_, ctx) in &sessions {
                if let Some(l1) = ctx.l1_transport.as_ref() {
                    l1.update_position(update.x, update.y, update.z);
                }
            }
        }

        for (room_id, update) in per_room_updates {
            if let Some((_, ctx)) = sessions.iter().find(|(id, _)| *id == room_id) {
                if let Some(l1) = ctx.l1_transport.as_ref() {
                    l1.update_position(update.x, update.y, update.z);
                }
            }
        }
    }
}

async fn update_stats_cache() {
    let stats_json = ENGINE.get_stats_json().await;
    let mut cache = STATS_CACHE.lock().unwrap();
    *cache = stats_json;
}

pub fn get_stats() -> String {
    STATS_CACHE.lock().unwrap().clone()
}

pub fn get_config() -> String {
    ENGINE.l0.get_config().to_json_string()
}

pub fn storage_add(name: &str, data: &[u8]) -> mistlib_core::error::Result<String> {
    ENGINE.runtime.block_on(ENGINE.l0.storage_add(name, data))
}

/// Explicit-position variant of `storage_add` (SPEC-16).
pub fn storage_add_at(
    name: &str,
    data: &[u8],
    position: Option<mistlib_core::types::Vector3>,
) -> mistlib_core::error::Result<String> {
    ENGINE
        .runtime
        .block_on(ENGINE.l0.storage_add_at(name, data, position))
}

pub fn storage_get(cid: &str) -> mistlib_core::error::Result<Vec<u8>> {
    ENGINE.runtime.block_on(ENGINE.l0.storage_get(cid))
}
