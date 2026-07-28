use crate::engine::{SessionCtx, ENGINE};
use mistlib_core::types::NodeId;
use std::sync::Arc;

pub type EventCallback = unsafe extern "C" fn(u32, *const u8, usize, *const u8, usize);
/// Same shape as `EventCallback`, with a room_id (ptr, len) pair inserted
/// immediately after `message_type` -- following the crate's existing
/// ptr+len string convention rather than a null-terminated `c_char*` (see
/// SPEC-15).
pub type EventCallbackV2 =
    unsafe extern "C" fn(u32, *const u8, usize, *const u8, usize, *const u8, usize);
pub type RustEventCallback = Arc<dyn Fn(u32, String, Vec<u8>) + Send + Sync + 'static>;

pub const EVENT_RAW: u32 = 0;
pub const EVENT_OVERLAY: u32 = 1;
pub const EVENT_JOIN: u32 = 2;
pub const EVENT_LEAVE: u32 = 3;
pub const EVENT_NEIGHBORS: u32 = 4;
pub const EVENT_AOI_ENTERED: u32 = 5;
pub const EVENT_AOI_LEFT: u32 = 6;
pub const EVENT_NODE_POSITION_UPDATED: u32 = 7;
pub const EVENT_AOI_NODES: u32 = 8;
pub const EVENT_ALL_CONNECTIONS_LOST: u32 = 9;

pub fn dispatch_event(message_type: u32, room_id: &str, from: &NodeId, data: &[u8]) {
    if ENGINE
        .event_dispatch_tx
        .send((
            message_type,
            room_id.to_string(),
            from.clone(),
            data.to_vec(),
        ))
        .is_err()
    {
        tracing::debug!("dropping FFI event for {from}: dispatch thread stopped");
        mistlib_core::stats::STATS.add_dropped_ffi_event();
    }
}

async fn mark_connected(ctx: &Arc<SessionCtx>, node_id: NodeId) {
    ctx.ensure_node_registered(&node_id);
    if let Some(overlay) = ctx.overlay.as_ref() {
        let mut rt = overlay.routing_table.lock().unwrap();
        rt.on_connected(node_id.clone());
    }
    dispatch_event(EVENT_JOIN, &ctx.room_id, &node_id, b"joined");
}

/// Marks `node_id` connected within a specific room's session. This is the
/// path used by `WebRtcTransport`'s own ICE/data-channel handlers (see
/// `transports/webrtc/peer.rs`), which always know their own room.
pub fn on_connected_internal(room_id: String, node_id: NodeId) {
    ENGINE.runtime.spawn(async move {
        if let Some(ctx) = ENGINE.get_session(&room_id).await {
            mark_connected(&ctx, node_id).await;
        }
    });
}

/// Roomless variant for the host-facing FFI `on_connected` export, which has
/// no room of its own to scope to: applies to the first-joined session
/// (SPEC-15 rule 5's "fallback" convention).
pub fn on_connected_primary(node_id: NodeId) {
    ENGINE.runtime.spawn(async move {
        if let Some(ctx) = ENGINE.primary_session().await {
            mark_connected(&ctx, node_id).await;
        } else {
            tracing::debug!("on_connected: no active session, dropping notification");
        }
    });
}

async fn mark_disconnected(ctx: Option<&Arc<SessionCtx>>, room_id: &str, node_id: &NodeId) {
    if let Some(ctx) = ctx {
        if let Some(overlay) = ctx.overlay.as_ref() {
            let mut rt = overlay.routing_table.lock().unwrap();
            rt.on_disconnected(node_id);
        }
    }
    mistlib_core::stats::STATS.remove_rtt(node_id);
    dispatch_event(EVENT_LEAVE, room_id, node_id, b"left");
}

/// Session-scoped counterpart to `on_connected_internal` (see there for why
/// the WebRTC-internal path always knows its room_id).
pub fn on_disconnected_internal(room_id: String, node_id: NodeId) {
    ENGINE.runtime.spawn(async move {
        let ctx = ENGINE.get_session(&room_id).await;
        mark_disconnected(ctx.as_ref(), &room_id, &node_id).await;

        // Trigger an immediate re-selection instead of waiting for the next
        // periodic balancer tick (up to ~2.4s), so recovery from a confirmed
        // disconnect isn't gated by tick cadence. The transport's own
        // reconnect cooldown (RECONNECT_COOLDOWN_MS) still rate-limits the
        // actual retry.
        request_immediate_rebalance(ctx);
    });
}

/// Roomless counterpart to `on_connected_primary`.
pub fn on_disconnected_primary(node_id: NodeId) {
    ENGINE.runtime.spawn(async move {
        let ctx = ENGINE.primary_session().await;
        if ctx.is_none() {
            tracing::debug!("on_disconnected: no active session, dropping notification");
        }
        let room_id = ctx.as_ref().map(|c| c.room_id.clone()).unwrap_or_default();
        mark_disconnected(ctx.as_ref(), &room_id, &node_id).await;
        request_immediate_rebalance(ctx);
    });
}

fn request_immediate_rebalance(ctx: Option<Arc<SessionCtx>>) {
    let Some(ctx) = ctx else { return };
    let Some(overlay) = ctx.overlay.as_ref() else {
        return;
    };
    let Some(wt) = ctx.webrtc_transport.as_ref() else {
        return;
    };
    let states = wt.get_active_connection_states();
    let config = ENGINE.config.lock().unwrap().clone();
    for action in overlay.notify_peer_disconnected(&config, &states) {
        ENGINE.handle_action_for(ctx.clone(), action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, LazyLock, Mutex};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct CallbackState {
        seen: Vec<(u32, String, Vec<u8>)>,
        block_next: bool,
    }

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    static CALLBACK_STATE: LazyLock<(Mutex<CallbackState>, Condvar)> =
        LazyLock::new(|| (Mutex::new(CallbackState::default()), Condvar::new()));

    type RustCallbackSeen = Vec<(u32, String, Vec<u8>)>;
    static RUST_CALLBACK_STATE: LazyLock<(Mutex<RustCallbackSeen>, Condvar)> =
        LazyLock::new(|| (Mutex::new(Vec::new()), Condvar::new()));

    unsafe extern "C" fn blocking_test_callback(
        message_type: u32,
        from_ptr: *const u8,
        from_len: usize,
        data_ptr: *const u8,
        data_len: usize,
    ) {
        let from =
            String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(from_ptr, from_len) })
                .to_string();
        let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) }.to_vec();

        let should_block = {
            let (lock, _) = &*CALLBACK_STATE;
            let mut state = lock.lock().unwrap();
            let should_block = state.block_next;
            state.block_next = false;
            should_block
        };

        if should_block {
            std::thread::sleep(Duration::from_millis(100));
        }

        let (lock, cvar) = &*CALLBACK_STATE;
        let mut state = lock.lock().unwrap();
        state.seen.push((message_type, from, data));
        cvar.notify_all();
    }

    #[test]
    fn dispatch_event_invokes_rust_callback_without_ffi() {
        let _guard = TEST_LOCK.lock().unwrap();
        {
            let (lock, _) = &*RUST_CALLBACK_STATE;
            lock.lock().unwrap().clear();
        }
        *ENGINE.global_callback.lock().unwrap() = None;
        *ENGINE.rust_event_callback.lock().unwrap() = Some(Arc::new(|message_type, from, data| {
            let (lock, cvar) = &*RUST_CALLBACK_STATE;
            let mut seen = lock.lock().unwrap();
            seen.push((message_type, from, data));
            cvar.notify_all();
        }));

        dispatch_event(
            EVENT_RAW,
            "test-room",
            &NodeId("peer-rust".to_string()),
            b"hello",
        );

        let (lock, cvar) = &*RUST_CALLBACK_STATE;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut seen = lock.lock().unwrap();
        while seen.is_empty() {
            let now = Instant::now();
            assert!(now < deadline, "timed out waiting for Rust event callback");
            let wait = deadline.saturating_duration_since(now);
            let (next_seen, _) = cvar.wait_timeout(seen, wait).unwrap();
            seen = next_seen;
        }

        assert_eq!(seen[0].0, EVENT_RAW);
        assert_eq!(seen[0].1, "peer-rust");
        assert_eq!(seen[0].2, b"hello");

        *ENGINE.rust_event_callback.lock().unwrap() = None;
    }

    #[test]
    fn dispatch_event_queues_while_callback_is_blocked_and_preserves_order() {
        let _guard = TEST_LOCK.lock().unwrap();
        *ENGINE.rust_event_callback.lock().unwrap() = None;
        {
            let (lock, _) = &*CALLBACK_STATE;
            let mut state = lock.lock().unwrap();
            state.seen.clear();
            state.block_next = true;
        }
        *ENGINE.global_callback.lock().unwrap() = Some(blocking_test_callback);

        for index in 0..5u32 {
            let from = NodeId(format!("peer-{index}"));
            let payload = format!("payload-{index}");
            dispatch_event(EVENT_RAW + index, "test-room", &from, payload.as_bytes());
        }

        let (lock, cvar) = &*CALLBACK_STATE;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut state = lock.lock().unwrap();
        while state.seen.len() < 5 {
            let now = Instant::now();
            assert!(now < deadline, "timed out waiting for queued FFI events");
            let wait = deadline.saturating_duration_since(now);
            let (next_state, _) = cvar.wait_timeout(state, wait).unwrap();
            state = next_state;
        }

        for index in 0..5u32 {
            let event = &state.seen[index as usize];
            assert_eq!(event.0, EVENT_RAW + index);
            assert_eq!(event.1, format!("peer-{index}"));
            assert_eq!(event.2, format!("payload-{index}").as_bytes());
        }

        *ENGINE.global_callback.lock().unwrap() = None;
    }
}
