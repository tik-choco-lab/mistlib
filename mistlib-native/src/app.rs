use bytes::Bytes;
use std::sync::LazyLock;
use tracing_subscriber::{prelude::*, EnvFilter};

use crate::engine::*;
use mistlib_core::layers::L0Engine;
use mistlib_core::types::{DeliveryMethod, NodeId};

pub const DELIVERY_RELIABLE: u32 = 0;
pub const DELIVERY_UNRELIABLE_ORDERED: u32 = 1;
pub const DELIVERY_UNRELIABLE: u32 = 2;

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

pub fn register_log_callback(cb: LogCallback) {
    let mut callback = ENGINE.log_callback.lock().unwrap();
    *callback = Some(cb);
}

pub fn register_event_callback(cb: EventCallback) {
    let mut callback = ENGINE.global_callback.lock().unwrap();
    *callback = Some(cb);
}

pub fn init(id: String, signaling_url: String) {
    let _ = *INIT_LOG;
    tracing::info!("mistlib::init called");

    let local_id = NodeId(id);
    ENGINE.l0.initialize(local_id, signaling_url);
}

pub fn leave_room() {
    ENGINE.l0.leave_room();
}

pub fn update_position(x: f32, y: f32, z: f32) {
    ENGINE.runtime.spawn(async move {
        if let Some(ctx) = ENGINE.get_context().await {
            if let Some(l1) = ctx.l1_transport.as_ref() {
                l1.update_position(x, y, z);
            }
        }
    });
}

pub fn on_connected(node_id: NodeId) {
    on_connected_internal(node_id);
}

pub fn on_disconnected(node_id: NodeId) {
    on_disconnected_internal(node_id);
}

pub fn set_config(data: &[u8]) {
    if let Ok(json_str) = std::str::from_utf8(data) {
        let mut config = ENGINE.l0.get_config();
        if config.update_from_json(json_str) {
            ENGINE.l0.set_config(config);
        }
    }
}

pub fn send_message(target_id: String, data: &[u8], method: u32) {
    let target_node = NodeId(target_id);
    let bytes = Bytes::copy_from_slice(data);

    let delivery = match method {
        DELIVERY_RELIABLE => DeliveryMethod::ReliableOrdered,
        DELIVERY_UNRELIABLE_ORDERED => DeliveryMethod::UnreliableOrdered,
        _ => DeliveryMethod::Unreliable,
    };

    ENGINE.runtime.spawn(async move {
        if let Some(ctx) = ENGINE.get_context().await {
            if let Some(l1) = ctx.l1_transport.as_ref() {
                if target_node.0.is_empty() {
                    let _ = l1.broadcast(bytes, delivery).await;
                } else {
                    let _ = l1.send_message(&target_node, bytes, delivery).await;
                }
            }
        }
    });
}

pub fn get_stats() -> String {
    ENGINE.runtime.block_on(async {
        let stats = ENGINE.l0.get_stats().await;
        serde_json::to_string(&stats).unwrap_or_else(|_| "{}".to_string())
    })
}

pub fn get_config() -> String {
    ENGINE.l0.get_config().to_json_string()
}
