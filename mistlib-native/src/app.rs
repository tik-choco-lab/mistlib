use bytes::Bytes;
use std::sync::{LazyLock, Mutex};
use tracing_subscriber::{prelude::*, EnvFilter};
use tokio::sync::mpsc;

use crate::engine::*;
use mistlib_core::layers::L0Engine;
use mistlib_core::types::{DeliveryMethod, NodeId};

pub const DELIVERY_RELIABLE: u32 = 0;
pub const DELIVERY_UNRELIABLE_ORDERED: u32 = 1;
pub const DELIVERY_UNRELIABLE: u32 = 2;

#[derive(Clone)]
struct SendRequest {
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
static POSITION_CACHE: LazyLock<Mutex<Option<PositionUpdate>>> = LazyLock::new(|| Mutex::new(None));
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
    ensure_send_worker();
    ensure_stats_worker();
}

pub fn leave_room() {
    ENGINE.l0.leave_room();
}

pub fn update_position(x: f32, y: f32, z: f32) {
    {
        let mut cache = POSITION_CACHE.lock().unwrap();
        *cache = Some(PositionUpdate { x, y, z });
    }

    ensure_position_worker();
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
    let Some(sender) = SEND_QUEUE.lock().unwrap().clone() else {
        tracing::warn!("send_message dropped: send worker is not initialized");
        return;
    };

    let target_node = NodeId(target_id);
    let bytes = Bytes::copy_from_slice(data);

    let delivery = match method {
        DELIVERY_RELIABLE => DeliveryMethod::ReliableOrdered,
        DELIVERY_UNRELIABLE_ORDERED => DeliveryMethod::UnreliableOrdered,
        _ => DeliveryMethod::Unreliable,
    };

    if let Err(err) = sender.try_send(SendRequest {
        target_node,
        bytes,
        delivery,
    }) {
        tracing::warn!("send_message queue full or closed: {}", err);
    }
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

async fn send_worker(mut rx: mpsc::Receiver<SendRequest>) {
    let mut cached_generation = u64::MAX;
    let mut cached_ctx: Option<std::sync::Arc<RunningContext>> = None;

    while let Some(req) = rx.recv().await {
        let current_generation = ENGINE.context_generation.load(std::sync::atomic::Ordering::Relaxed);
        if cached_ctx.is_none() || cached_generation != current_generation {
            cached_ctx = ENGINE.get_context().await;
            cached_generation = current_generation;
        }

        if let Some(ctx) = cached_ctx.as_ref() {
            if let Some(l1) = ctx.l1_transport.as_ref() {
                if req.target_node.0.is_empty() {
                    let _ = l1.broadcast(req.bytes, req.delivery).await;
                } else {
                    let _ = l1.send_message(&req.target_node, req.bytes, req.delivery).await;
                }
            }
        }
    }
}

async fn stats_worker() {
    
    update_stats_cache().await;

    let mut interval = tokio::time::interval(web_time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        update_stats_cache().await;
    }
}

async fn position_worker() {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(50));
    let mut cached_generation = u64::MAX;
    let mut cached_ctx: Option<std::sync::Arc<RunningContext>> = None;

    loop {
        interval.tick().await;

        let update = {
            let mut cache = POSITION_CACHE.lock().unwrap();
            cache.take()
        };

        let Some(update) = update else {
            continue;
        };

        let current_generation = ENGINE.context_generation.load(std::sync::atomic::Ordering::Relaxed);
        if cached_ctx.is_none() || cached_generation != current_generation {
            cached_ctx = ENGINE.get_context().await;
            cached_generation = current_generation;
        }

        if let Some(ctx) = cached_ctx.as_ref() {
            if let Some(l1) = ctx.l1_transport.as_ref() {
                l1.update_position(update.x, update.y, update.z);
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
