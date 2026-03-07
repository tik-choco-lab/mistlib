use crate::engine::{EngineState, ENGINE};
use mistlib_core::types::NodeId;

pub type EventCallback = unsafe extern "C" fn(u32, *const u8, usize, *const u8, usize);

pub const EVENT_RAW: u32 = 0;
pub const EVENT_OVERLAY: u32 = 1;
pub const EVENT_JOIN: u32 = 2;
pub const EVENT_LEAVE: u32 = 3;
pub const EVENT_NEIGHBORS: u32 = 4;
pub const EVENT_AOI_ENTERED: u32 = 5;
pub const EVENT_AOI_LEFT: u32 = 6;
pub const EVENT_NODE_POSITION_UPDATED: u32 = 7;

pub fn dispatch_event(message_type: u32, from: &NodeId, data: &[u8]) {
    if let Ok(callback_lock) = ENGINE.global_callback.try_lock() {
        if let Some(cb) = *callback_lock {
            unsafe {
                cb(
                    message_type,
                    from.0.as_ptr(),
                    from.0.len(),
                    data.as_ptr(),
                    data.len(),
                );
            }
        }
    }
}

pub fn on_connected_internal(node_id: NodeId) {
    ENGINE.runtime.spawn(async move {
        let state_lock = ENGINE.state.read().await;
        if let EngineState::Running(ctx) = &*state_lock {
            if let Some(overlay) = ctx.overlay.as_ref() {
                let mut rt = overlay.routing_table.lock().unwrap();
                rt.on_connected(node_id.clone());
            }
        }
        dispatch_event(EVENT_JOIN, &node_id, b"joined");
    });
}

pub fn on_disconnected_internal(node_id: NodeId) {
    ENGINE.runtime.spawn(async move {
        {
            let mut store = ENGINE.node_store.lock().unwrap();
            store.nodes.remove(&node_id);
            store.last_updated.remove(&node_id);
        }
        let state_lock = ENGINE.state.read().await;
        if let EngineState::Running(ctx) = &*state_lock {
            if let Some(overlay) = ctx.overlay.as_ref() {
                let mut rt = overlay.routing_table.lock().unwrap();
                rt.on_disconnected(&node_id);
            }
        }
        mistlib_core::stats::STATS.remove_rtt(&node_id);
        dispatch_event(EVENT_LEAVE, &node_id, b"left");
    });
}
