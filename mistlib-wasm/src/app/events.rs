use super::{
    EVENT_AOI_ENTERED, EVENT_AOI_LEFT, EVENT_AOI_NODES, EVENT_CALLBACK, EVENT_NEIGHBORS,
    EVENT_OVERLAY, EVENT_PEER_CONNECTED, EVENT_PEER_DISCONNECTED, EVENT_RAW, EVENT_ROOM_JOINED,
    EVENT_ROOM_JOIN_FAILED, EVENT_ROOM_LEFT,
};
use mistlib_core::engine::{EngineEvent, EngineEventHandler};
use mistlib_core::types::NodeId;
use wasm_bindgen::prelude::*;

/// One per session (see `layers::wasm_l0::build_session`), closing over that
/// session's `room_id` so both the JS event callback and storage-control
/// replies know which room a message arrived on (multi-room contract point
/// 9: dispatch appends room_id as a 4th argument).
pub(crate) struct WasmEngineEventHandler {
    room_id: String,
}

impl WasmEngineEventHandler {
    pub(crate) fn new(room_id: String) -> Self {
        Self { room_id }
    }
}

impl EngineEventHandler for WasmEngineEventHandler {
    fn on_event(&self, event: EngineEvent) {
        if let EngineEvent::RawMessage(from, data) = &event {
            handle_storage_control_message(&self.room_id, from, data);
        }

        let callback = EVENT_CALLBACK.with(|cb| cb.borrow().as_ref().cloned());
        if let Some(f) = callback {
            let room_id = self.room_id.clone();
            let (event_type, from_id, payload_vec) = match event {
                EngineEvent::RawMessage(id, data) => (EVENT_RAW, id.0, data.to_vec()),
                EngineEvent::OverlayMessage(id, data) => (EVENT_OVERLAY, id.0, data),
                EngineEvent::NeighborsUpdated(data) => (EVENT_NEIGHBORS, "rust".to_string(), data),
                EngineEvent::AoiEntered(id) => (EVENT_AOI_ENTERED, id.0, vec![]),
                EngineEvent::AoiLeft(id) => (EVENT_AOI_LEFT, id.0, vec![]),
                EngineEvent::AoiNodesUpdated(data) => (EVENT_AOI_NODES, "rust".to_string(), data),
            };
            wasm_bindgen_futures::spawn_local(async move {
                dispatch_event(&f, event_type, &from_id, &payload_vec, &room_id);
            });
        }
    }
}

/// Invokes the registered JS event callback via `Function::apply` with a
/// 4-element args array (event_type, from_id, payload, room_id). Existing
/// 3-arg JS callbacks simply ignore the extra `room_id` argument, so this
/// stays backward compatible with callbacks registered before multi-room
/// support (multi-room contract point 9).
fn dispatch_event(
    f: &js_sys::Function,
    event_type: u32,
    from_id: &str,
    payload: &[u8],
    room_id: &str,
) {
    let args = js_sys::Array::new();
    args.push(&JsValue::from_f64(event_type as f64));
    args.push(&JsValue::from_str(from_id));
    args.push(&js_sys::Uint8Array::from(payload).into());
    args.push(&JsValue::from_str(room_id));
    let _ = f.apply(&JsValue::NULL, &args);
}

fn handle_storage_control_message(room_id: &str, from: &NodeId, data: &[u8]) {
    let Some((&first, rest)) = data.split_first() else {
        return;
    };

    match first {
        0x01 => {
            if let Ok(cid) = std::str::from_utf8(rest) {
                crate::storage::handle_want(room_id.to_string(), from.clone(), cid.to_string());
            }
        }
        0x02 => handle_storage_have(rest),
        0x03 => {
            if let Ok(cid) = std::str::from_utf8(rest) {
                crate::storage::handle_query(room_id.to_string(), from.clone(), cid.to_string());
            }
        }
        0x04 => {
            if let Ok(cid) = std::str::from_utf8(rest) {
                crate::storage::handle_have_status(from.clone(), cid.to_string());
            }
        }
        0x05 => {
            if let Some((cid, chunk_index, chunk_total, payload)) =
                crate::storage::resolver::parse_have_chunk_message(data)
            {
                crate::storage::WANT_REGISTRY.with(|r| {
                    r.fulfill_chunk(&cid, chunk_index, chunk_total, payload);
                });
            }
        }
        _ => {}
    }
}

fn handle_storage_have(rest: &[u8]) {
    let Some((&cid_len, body)) = rest.split_first() else {
        return;
    };
    let cid_len = cid_len as usize;
    if body.len() < cid_len {
        return;
    }

    if let Ok(cid) = std::str::from_utf8(&body[..cid_len]) {
        let payload = body[cid_len..].to_vec();
        crate::storage::handle_have(cid.to_string(), payload);
    }
}

pub fn emit_peer_connected(node_id: NodeId, room_id: String) {
    let callback = EVENT_CALLBACK.with(|cb| cb.borrow().as_ref().cloned());
    if let Some(f) = callback {
        wasm_bindgen_futures::spawn_local(async move {
            dispatch_event(&f, EVENT_PEER_CONNECTED, &node_id.0, &[], &room_id);
        });
    }
}

pub fn emit_peer_disconnected(node_id: NodeId, room_id: String) {
    // Per-session (multi-room contract point 9): only the affected room's
    // transport/engine react, not every joined room's.
    if let Some(webrtc) = crate::app::session_webrtc(&room_id) {
        webrtc.schedule_isolation_recovery();
    }
    // Trigger an immediate re-selection instead of waiting for the next
    // periodic balancer tick (up to ~2.4s), so recovery from a confirmed
    // disconnect isn't gated by tick cadence. The transport's own reconnect
    // handling (request_peers/connect) still governs the actual retry.
    if let Some(engine) = crate::app::session_engine(&room_id) {
        engine.notify_peer_disconnected();
    }
    let callback = EVENT_CALLBACK.with(|cb| cb.borrow().as_ref().cloned());
    if let Some(f) = callback {
        wasm_bindgen_futures::spawn_local(async move {
            dispatch_event(&f, EVENT_PEER_DISCONNECTED, &node_id.0, &[], &room_id);
        });
    }
}

/// Fires once a `join_room`/`join_room_async` call for `room_id` has left
/// the room in a usable state -- either because a brand new session was just
/// inserted, or because the room was already active and this call was just
/// an idempotent re-announce. This is deliberate: event-gated callers must
/// get a signal on re-join too, so `EVENT_ROOM_JOINED` may fire more than
/// once per room over the lifetime of a page, once per join call that finds
/// the room usable.
///
/// Dispatch to the JS callback is always `spawn_local`-deferred (see
/// `dispatch_event` below), so there is a window between the moment this is
/// called and the moment JS actually observes it. An intervening
/// `leave_room`/`leave_room_id` can land inside that window and queue
/// `EVENT_ROOM_LEFT` right behind it -- but never ahead of it, since both
/// events for the same room_id are dispatched via the same single-threaded
/// `spawn_local` queue, so callback delivery stays FIFO. If a caller needs
/// the *current* readiness synchronously rather than waiting on eventual
/// callback delivery, `is_room_joined` (backed by `session_exists`, which is
/// updated synchronously, not deferred) is the source of truth.
pub fn emit_room_joined(room_id: String) {
    let callback = EVENT_CALLBACK.with(|cb| cb.borrow().as_ref().cloned());
    if let Some(f) = callback {
        wasm_bindgen_futures::spawn_local(async move {
            dispatch_event(&f, EVENT_ROOM_JOINED, "", &[], &room_id);
        });
    }
}

/// Fires when a `join_room`/`join_room_async` call for `room_id` fails to
/// produce a usable session -- either `build_session` itself errored, or an
/// intervening `leave_room`/`leave_room_id` cancelled the build before it
/// could be inserted. `reason` is a human-readable string, sent as UTF-8
/// bytes in the event payload.
pub fn emit_room_join_failed(room_id: String, reason: String) {
    let callback = EVENT_CALLBACK.with(|cb| cb.borrow().as_ref().cloned());
    if let Some(f) = callback {
        wasm_bindgen_futures::spawn_local(async move {
            dispatch_event(&f, EVENT_ROOM_JOIN_FAILED, "", reason.as_bytes(), &room_id);
        });
    }
}

/// Fires after `room_id`'s session has actually been torn down, whether via
/// `leave_room_id` (one room) or `leave_room` (every joined room). Not fired
/// for a leave that only cancelled a still-in-flight join (that build
/// finishes and emits `EVENT_ROOM_JOIN_FAILED` instead, since no session was
/// ever inserted).
pub fn emit_room_left(room_id: String) {
    let callback = EVENT_CALLBACK.with(|cb| cb.borrow().as_ref().cloned());
    if let Some(f) = callback {
        wasm_bindgen_futures::spawn_local(async move {
            dispatch_event(&f, EVENT_ROOM_LEFT, "", &[], &room_id);
        });
    }
}

pub fn register_event_callback(callback: &js_sys::Function) {
    EVENT_CALLBACK.with(|cb| {
        *cb.borrow_mut() = Some(callback.clone());
    });
}
