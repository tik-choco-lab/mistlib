mod events;
mod media;
mod state;
#[cfg(test)]
mod tests;

use crate::layers::wasm_l0::WasmL0;
use crate::transport::webrtc::WasmWebRtcTransport;
use bytes::Bytes;
use mistlib_core::config::Config;
use mistlib_core::engine::{EngineState, MistEngine, RunningContext};
use mistlib_core::layers::L0Engine;
use mistlib_core::signaling::MessageContent;
use mistlib_core::transport::Transport;
use mistlib_core::types::DeliveryMethod;
use mistlib_core::types::NodeId;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use wasm_bindgen::JsValue;

pub(crate) use events::WasmEngineEventHandler;
pub use events::{
    emit_peer_connected, emit_peer_disconnected, emit_room_join_failed, emit_room_joined,
    emit_room_left, register_event_callback,
};
pub use media::{
    emit_media_track_added, emit_media_track_removed, get_local_track, publish_local_track,
    register_local_track, register_media_event_callback, remove_local_track,
    set_local_track_enabled, unpublish_local_track,
};
pub(crate) use state::*;

/// A full per-room stack: its own signaler connection, WebRTC transport and
/// core `MistEngine` (overlay/DNVE3), keyed by room_id in `SESSIONS`. See the
/// multi-room contract in `mistlib-wasm/src/layers/wasm_l0.rs` for how these
/// are built (`build_session`) and torn down (`teardown_session`).
pub(crate) struct Session {
    pub engine: Arc<MistEngine>,
    pub webrtc: Arc<WasmWebRtcTransport>,
    #[allow(dead_code)]
    // kept alive for parity with the pre-multi-room singleton; not yet read back.
    pub l1_transport: Arc<crate::layers::wasm_l1::WasmL1Transport>,
}

pub(crate) fn all_session_engines() -> Vec<Arc<MistEngine>> {
    SESSIONS.with(|s| {
        s.borrow()
            .iter_in_join_order()
            .map(|(_, sess)| sess.engine.clone())
            .collect()
    })
}

pub(crate) fn first_session_engine() -> Option<Arc<MistEngine>> {
    SESSIONS.with(|s| s.borrow().first().map(|(_, sess)| sess.engine.clone()))
}

pub(crate) fn session_engine(room_id: &str) -> Option<Arc<MistEngine>> {
    SESSIONS.with(|s| s.borrow().get(room_id).map(|sess| sess.engine.clone()))
}

/// Returns the engine's `RunningContext` if it is currently running.
pub(crate) fn running_ctx(engine: &Arc<MistEngine>) -> Option<Arc<RunningContext>> {
    let state = engine.state.lock().unwrap();
    if let EngineState::Running(ctx) = &*state {
        Some(ctx.clone())
    } else {
        None
    }
}

pub(crate) fn session_running_ctx(room_id: &str) -> Option<Arc<RunningContext>> {
    session_engine(room_id).and_then(|e| running_ctx(&e))
}

/// Snapshots the `Transport` (overlay transport) of every running session,
/// e.g. so the storage resolver can fan a WANT/QUERY broadcast out across
/// every joined room instead of a single captured transport.
pub(crate) fn all_session_transports() -> Vec<Arc<dyn Transport>> {
    all_session_engines()
        .iter()
        .filter_map(running_ctx)
        .map(|ctx| ctx.transport.clone())
        .collect()
}

pub(crate) fn all_session_webrtc_transports() -> Vec<Arc<WasmWebRtcTransport>> {
    SESSIONS.with(|s| {
        s.borrow()
            .iter_in_join_order()
            .map(|(_, sess)| sess.webrtc.clone())
            .collect()
    })
}

/// Whether `engine`'s session already knows a route to `target` (directly in
/// its node store, or via its overlay routing table), used to pick which
/// session's transport a unicast `send_message` should go out on.
fn session_knows_target(engine: &Arc<MistEngine>, target: &NodeId) -> bool {
    if engine.node_store.lock().unwrap().nodes.contains_key(target) {
        return true;
    }
    running_ctx(engine)
        .and_then(|ctx| ctx.overlay.clone())
        .is_some_and(|ov| {
            ov.routing_table
                .lock()
                .unwrap()
                .get_next_hop(target)
                .is_some()
        })
}

pub const EVENT_RAW: u32 = 0;
pub const EVENT_OVERLAY: u32 = 1;
pub const EVENT_NEIGHBORS: u32 = 2;
pub const EVENT_AOI_ENTERED: u32 = 3;
pub const EVENT_AOI_LEFT: u32 = 4;
pub const EVENT_PEER_CONNECTED: u32 = 5;
pub const EVENT_PEER_DISCONNECTED: u32 = 6;
pub const EVENT_AOI_NODES: u32 = 7;
pub const EVENT_ROOM_JOINED: u32 = 8;
pub const EVENT_ROOM_JOIN_FAILED: u32 = 9;
pub const EVENT_ROOM_LEFT: u32 = 10;
pub const MEDIA_EVENT_TRACK_ADDED: u32 = 100;
pub const MEDIA_EVENT_TRACK_REMOVED: u32 = 101;
pub use mistlib_core::types::{
    DELIVERY_RELIABLE, DELIVERY_UNRELIABLE, DELIVERY_UNRELIABLE_ORDERED,
};

/// Overlay `message_type` for the app-level position-sync broadcast sent by
/// `update_position()`. Distinct from `mistlib_core::overlay::OVERLAY_MSG_*`
/// (0-5, internal DNVE3 control), from the legacy JSON `sync_pos` type (100,
/// still special-cased in `mistlib_core::engine::network` for backward compat
/// but no longer emitted by this crate), and from the unrelated
/// `MEDIA_EVENT_*` JS-callback event-type tags above (a different namespace).
/// Payload is a bare bincode-serialized `Vector3` (12 bytes); `app::events`
/// is the only reader and assumes nothing else uses this message type.
pub(crate) const MSG_TYPE_POSITION_SYNC_BIN: u32 = 200;

fn init_runtime_once() {
    console_error_panic_hook::set_once();

    static TRACING_INIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !TRACING_INIT.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let config = tracing_wasm::WASMLayerConfigBuilder::new()
            .set_max_level(tracing::Level::INFO)
            .build();
        tracing_wasm::set_as_global_default_with_config(config);
    }
}

fn config_from_json(data: &str) -> Option<Config> {
    let mut config = if let Ok(config) = serde_json::from_str::<Config>(data) {
        config
    } else {
        let mut config = base_config();
        if let Err(err) = config.update_from_json(data) {
            tracing::error!("config_from_json: update_from_json rejected input: {}", err);
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

pub fn init(id: String, url: String) {
    init_runtime_once();

    tracing::info!("Initializing Mist Engine WASM bindings for {}", id);
    let l0 = WasmL0::new();
    l0.initialize(NodeId(id), url);
}

pub fn init_with_config(id: String, data: String) -> bool {
    init_runtime_once();

    let Some(config) = config_from_json(&data) else {
        tracing::warn!("init_with_config ignored invalid config JSON");
        return false;
    };
    let signaling_url = config.signaling_url.clone();

    tracing::info!("Initializing Mist Engine WASM bindings for {}", id);
    let l0 = WasmL0::new();
    l0.set_config(config);
    l0.initialize(NodeId(id), signaling_url);
    true
}

/// Position-syncs `engine`'s session: updates its own node store entry and,
/// if running with connected peers, broadcasts the new position through its
/// overlay. Shared by `update_position` (all sessions) and
/// `update_position_in_room` (one session).
fn apply_update_position(engine: &Arc<MistEngine>, self_id: &NodeId, x: f32, y: f32, z: f32) {
    {
        let mut store = engine.node_store.lock().unwrap();
        store.nodes.insert(
            self_id.clone(),
            mistlib_core::overlay::node_store::NodeInfo {
                id: self_id.clone(),
                position: mistlib_core::overlay::dnve3::Vector3::new(x, y, z),
            },
        );
        store
            .last_updated
            .insert(self_id.clone(), web_time::Instant::now());
    }

    let Some(ctx) = running_ctx(engine) else {
        return;
    };
    if ctx.overlay.is_none() {
        return;
    }
    let connected_nodes = ctx.transport.get_connected_nodes();
    if connected_nodes.is_empty() {
        return;
    }

    let hop_count = engine.config.lock().unwrap().limits.hop_count;

    use mistlib_core::overlay::dnve3::Vector3;
    use mistlib_core::overlay::{OverlayEnvelope, OverlayMessage};

    let Ok(payload) = bincode::serialize(&Vector3::new(x, y, z)) else {
        return;
    };
    let envelope = OverlayEnvelope::new(
        self_id.clone(),
        NodeId::broadcast(),
        hop_count,
        MessageContent::Overlay(OverlayMessage {
            message_type: MSG_TYPE_POSITION_SYNC_BIN,
            payload,
        }),
    );

    let Ok(data) = mistlib_core::overlay::wire::serialize(&envelope) else {
        return;
    };
    for target in connected_nodes {
        use mistlib_core::overlay::ActionHandler;
        engine.handle_action(mistlib_core::action::OverlayAction::SendMessage {
            to: target,
            data: Bytes::from(data.clone()),
            method: DeliveryMethod::Unreliable,
        });
    }
}

pub fn update_position(x: f32, y: f32, z: f32) {
    let self_id = self_id();
    for engine in all_session_engines() {
        apply_update_position(&engine, &self_id, x, y, z);
    }
}

pub fn update_position_in_room(room_id: String, x: f32, y: f32, z: f32) -> Result<(), JsValue> {
    let Some(engine) = session_engine(&room_id) else {
        return Err(JsValue::from_str(&format!("Room not joined: {}", room_id)));
    };
    apply_update_position(&engine, &self_id(), x, y, z);
    Ok(())
}

/// Merges a `NodeStore::get_connected_nodes_json`/`get_all_nodes_json` array
/// into `merged`, keyed by `id` so the same peer seen from multiple sessions
/// is only counted once (multi-room contract: union deduped by NodeId).
fn merge_json_array_by_id(merged: &mut HashMap<String, serde_json::Value>, json: &str) {
    if let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(json) {
        for item in items {
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                merged.insert(id.to_string(), item);
            }
        }
    }
}

fn neighbors_json_for(engine: &Arc<MistEngine>) -> String {
    let connected_nodes = running_ctx(engine)
        .map(|ctx| ctx.transport.get_connected_nodes())
        .unwrap_or_default();
    let Ok(store) = engine.node_store.try_lock() else {
        return "[]".to_string();
    };
    let connected_set: std::collections::HashSet<_> = connected_nodes.into_iter().collect();
    store.get_connected_nodes_json(&connected_set)
}

fn all_nodes_json_for(engine: &Arc<MistEngine>) -> String {
    let connected_nodes = running_ctx(engine)
        .map(|ctx| ctx.transport.get_connected_nodes())
        .unwrap_or_default();
    let Ok(store) = engine.node_store.try_lock() else {
        return "[]".to_string();
    };
    let connected_set: std::collections::HashSet<_> = connected_nodes.into_iter().collect();
    store.get_all_nodes_json(&connected_set)
}

pub fn get_neighbors() -> String {
    let mut merged = HashMap::new();
    for engine in all_session_engines() {
        merge_json_array_by_id(&mut merged, &neighbors_json_for(&engine));
    }
    serde_json::to_string(&merged.into_values().collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".to_string())
}

pub fn get_all_nodes() -> String {
    let mut merged = HashMap::new();
    for engine in all_session_engines() {
        merge_json_array_by_id(&mut merged, &all_nodes_json_for(&engine));
    }
    serde_json::to_string(&merged.into_values().collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".to_string())
}

pub fn get_neighbors_in_room(room_id: String) -> Result<String, JsValue> {
    let Some(engine) = session_engine(&room_id) else {
        return Err(JsValue::from_str(&format!("Room not joined: {}", room_id)));
    };
    Ok(neighbors_json_for(&engine))
}

pub fn get_all_nodes_in_room(room_id: String) -> Result<String, JsValue> {
    let Some(engine) = session_engine(&room_id) else {
        return Err(JsValue::from_str(&format!("Room not joined: {}", room_id)));
    };
    Ok(all_nodes_json_for(&engine))
}

pub fn join_room(room_id: String) {
    let l0 = WasmL0::new();
    l0.join_room(room_id);
}

/// Awaitable counterpart to `join_room`: resolves (or rejects) once the room
/// is actually usable, instead of returning before the session has finished
/// building.
///
/// Deliberately a plain (non-`async`) fn returning `js_sys::Promise`, rather
/// than `pub async fn` (which is what this used to be): `reserve_join` -- the
/// pending mark / cancel-clear / waiter registration / already-active event,
/// see its doc comment -- runs synchronously here, before the `Promise` is
/// even constructed, so a `leave_room_id`/`leave_room` called in the same JS
/// tick right after this observes its effects instead of racing a deferred
/// future. Only the network-bound tail (`run_join`) happens inside the
/// `Promise`. See `layers::wasm_l0::reserve_join` for the full reasoning.
pub fn join_room_async(room_id: String) -> js_sys::Promise {
    let reservation = crate::layers::wasm_l0::reserve_join(&room_id);
    wasm_bindgen_futures::future_to_promise(async move {
        crate::layers::wasm_l0::run_join(room_id, reservation)
            .await
            .map(|_| JsValue::UNDEFINED)
    })
}

/// Whether `room_id` currently has an active session (i.e. `join_room`'s
/// build has finished and `EVENT_ROOM_JOINED` has already fired for it).
pub fn is_room_joined(room_id: String) -> bool {
    session_exists(&room_id)
}

pub fn leave_room() {
    let l0 = WasmL0::new();
    l0.leave_room();
}

pub fn leave_room_id(room_id: String) -> Result<(), JsValue> {
    crate::layers::wasm_l0::leave_room_id(&room_id)
}

/// Awaitable room leave used when the caller intends to rebuild the same
/// room/node identity. The reservation runs synchronously; the returned
/// promise resolves after the old signaling and transport cleanup finishes.
pub fn leave_room_id_async(room_id: String) -> js_sys::Promise {
    let reservation = crate::layers::wasm_l0::reserve_leave(&room_id);
    wasm_bindgen_futures::future_to_promise(async move {
        crate::layers::wasm_l0::run_leave(room_id, reservation)
            .await
            .map(|_| JsValue::UNDEFINED)
    })
}

pub fn set_config(data: String) -> bool {
    let Some(config) = config_from_json(&data) else {
        return false;
    };

    let l0 = WasmL0::new();
    l0.set_config(config);
    true
}

fn delivery_from(method: u32) -> DeliveryMethod {
    DeliveryMethod::from_u32(method)
}

/// Sends `bytes` to `target` through `ctx`'s overlay (falling back to a raw
/// send/broadcast if the overlay isn't up yet), routing through
/// `overlay.wrap_data` so envelope framing stays consistent regardless of
/// which session's transport ends up carrying it.
async fn send_via_ctx(
    ctx: &Arc<RunningContext>,
    target: &NodeId,
    bytes: Bytes,
    delivery: DeliveryMethod,
) {
    let Some(overlay) = ctx.overlay.as_ref() else {
        if target.is_broadcast() {
            let _ = ctx.transport.broadcast(bytes, delivery).await;
        } else {
            let _ = ctx.transport.send(target, bytes, delivery).await;
        }
        return;
    };

    let action = overlay.wrap_data(target, bytes, delivery);
    let mistlib_core::action::OverlayAction::SendMessage { to, data, method } = action else {
        return;
    };

    // `data` is already a serialized OverlayEnvelope (from wrap_data above), so it
    // must go out over the raw network transport, not `ctx.transport`
    // (`OverlayTransport`), which would wrap it a second time.
    let transport = ctx.preferred_transport();
    if to.is_broadcast() {
        let _ = transport.broadcast(data, method).await;
    } else {
        let _ = transport.send(&to, data, method).await;
    }
}

/// One pending `send_via_ctx` call, captured synchronously at
/// `send_message`/`send_message_in_room` call time -- see `enqueue_send`.
struct QueuedSend {
    ctx: Arc<RunningContext>,
    target: NodeId,
    bytes: Bytes,
    delivery: DeliveryMethod,
}

thread_local! {
    // FIFO of sends queued by `enqueue_send`, drained strictly one at a time
    // by the drainer task `enqueue_send` spawns -- see its doc comment for
    // why this exists instead of each caller awaiting `send_via_ctx` in its
    // own independently `spawn_local`-ed task.
    static SEND_QUEUE: RefCell<VecDeque<QueuedSend>> = RefCell::new(VecDeque::new());
    // Whether a drainer task is currently alive for SEND_QUEUE, so
    // `enqueue_send` never spawns more than one.
    static SEND_DRAINER_RUNNING: Cell<bool> = Cell::new(false);
}

/// Queues `(ctx, target, bytes, delivery)` for `send_via_ctx` and ensures a
/// single drainer task is (or stays) running to work through `SEND_QUEUE` in
/// order.
///
/// `send_message`/`send_message_in_room` used to `.await` `send_via_ctx`
/// directly from their own independently `spawn_local`-ed task per call.
/// `wasm_bindgen_futures`'s executor (`queue.rs`'s `run_all`) does drain
/// newly spawned tasks' *first* poll in strict FIFO order, and everything
/// before `overlay.wrap_data` (seq assignment, synchronous) runs with no real
/// `.await` in between -- so seq assignment itself stayed correctly ordered.
/// But `send_via_ctx`'s own `.await` -- ultimately `WasmWebRtcTransport::send`
/// -- genuinely yields under DataChannel backpressure
/// (`wait_for_buffered_amount_low`). Once an earlier call's task yields
/// there, a later call's already-queued task gets polled next in the same
/// `run_all` batch and can finish its own `dc.send_with_u8_array` (the actual
/// wire write) before the stalled earlier task resumes -- reordering bytes
/// on the wire even though `seq` was assigned in call order. Funneling every
/// send through this queue closes that window: the drainer never starts send
/// N+1's `send_via_ctx` call until send N's has fully resolved (backpressure
/// wait included), so both seq assignment and DataChannel writes stay in
/// call order.
///
/// Enqueueing itself is synchronous, so `send_message`/`send_message_in_room`
/// no longer need `spawn_local` at all for this part -- the call simply lands
/// in `SEND_QUEUE` in exact JS call order.
fn enqueue_send(ctx: Arc<RunningContext>, target: NodeId, bytes: Bytes, delivery: DeliveryMethod) {
    SEND_QUEUE.with(|q| {
        q.borrow_mut().push_back(QueuedSend {
            ctx,
            target,
            bytes,
            delivery,
        })
    });

    // Only the call that finds the drainer not already running spawns one;
    // an already-running drainer will pick up this entry on its next loop
    // iteration once it's done with whatever it's currently sending.
    if SEND_DRAINER_RUNNING.with(|r| r.replace(true)) {
        return;
    }

    wasm_bindgen_futures::spawn_local(async move {
        loop {
            let Some(job) = SEND_QUEUE.with(|q| q.borrow_mut().pop_front()) else {
                SEND_DRAINER_RUNNING.with(|r| r.set(false));
                return;
            };
            send_via_ctx(&job.ctx, &job.target, job.bytes, job.delivery).await;
        }
    });
}

pub fn send_message(target_id: String, data: &[u8], method: u32) {
    let bytes = Bytes::from(data.to_vec());
    // An empty target_id is the broadcast NodeId (NodeId::BROADCAST == "").
    let target = NodeId(target_id);
    let delivery = delivery_from(method);

    if target.is_broadcast() {
        for engine in all_session_engines() {
            if let Some(ctx) = running_ctx(&engine) {
                enqueue_send(ctx, target.clone(), bytes.clone(), delivery);
            }
        }
        return;
    }

    // Unicast: route via the first session (join order) whose
    // node_store/routing knows the target peer; fall back to the first
    // joined session.
    let engines = all_session_engines();
    let chosen = engines
        .iter()
        .find(|e| session_knows_target(e, &target))
        .cloned()
        .or_else(first_session_engine);
    let Some(engine) = chosen else { return };
    let Some(ctx) = running_ctx(&engine) else {
        return;
    };
    enqueue_send(ctx, target, bytes, delivery);
}

pub fn send_message_in_room(
    room_id: String,
    target_id: String,
    data: &[u8],
    method: u32,
) -> Result<(), JsValue> {
    let Some(engine) = session_engine(&room_id) else {
        return Err(JsValue::from_str(&format!("Room not joined: {}", room_id)));
    };

    let target = NodeId(target_id);
    let delivery = delivery_from(method);

    if let Some(ctx) = running_ctx(&engine) {
        enqueue_send(ctx, target, Bytes::from(data.to_vec()), delivery);
    }
    Ok(())
}

pub fn get_config() -> String {
    base_config().to_json_string()
}

pub fn get_stats() -> String {
    let snapshot = mistlib_core::stats::STATS.snapshot_and_reset();
    let rtt_millis: std::collections::HashMap<String, f32> = snapshot
        .rtt_millis
        .iter()
        .map(|(k, v)| (k.0.clone(), *v))
        .collect();

    let stats = serde_json::json!({
        "messageCount": snapshot.message_count,
        "sendBits": snapshot.send_bits,
        "receiveBits": snapshot.receive_bits,
        "rttMillis": rtt_millis,
        "worldSendBits": snapshot.world_send_bits,
        "worldReceiveBits": snapshot.world_receive_bits,
        "worldMessageCount": snapshot.world_message_count,
        "relaySendBits": snapshot.relay_send_bits,
        "relayReceiveBits": snapshot.relay_receive_bits,
        "relayMessageCount": snapshot.relay_message_count,
        "nodes": []
    });
    serde_json::to_string(&stats).unwrap_or_else(|_| "{}".to_string())
}
