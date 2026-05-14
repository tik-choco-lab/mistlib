use crate::engine::{EngineState, ENGINE};
use mistlib_core::signaling::Signaler;
use std::sync::atomic::Ordering;

pub(super) fn join_room(room_id: String) {
    ENGINE.runtime.spawn(set_room_id(room_id));
    ENGINE.runtime.spawn(start_engine_run());
    ENGINE.spawn_background_loops();
}

pub(super) fn leave_room() {
    ENGINE.runtime.spawn(shutdown_session());
    ENGINE.context_generation.fetch_add(1, Ordering::Relaxed);
}

async fn set_room_id(room_id: String) {
    if let Some(ctx) = ENGINE.get_context().await {
        if let Some(wt) = ctx.webrtc_transport.as_ref() {
            wt.set_room_id(room_id);
        }
    }
}

async fn start_engine_run() {
    let ctx_opt = {
        let state_lock = ENGINE.state.read().await;
        if let EngineState::Initialized(ctx) = &*state_lock {
            Some(ctx.clone())
        } else {
            None
        }
    };

    if let Some(ctx) = ctx_opt {
        if let Err(e) = ENGINE.run(ctx).await {
            tracing::error!("Engine run error: {}", e);
        }
    }
}

async fn shutdown_session() {
    let ctx_opt = {
        let state_lock = ENGINE.state.read().await;
        match &*state_lock {
            EngineState::Initialized(ctx) | EngineState::Running(ctx) => Some(ctx.clone()),
            EngineState::Idle => None,
        }
    };

    if let Some(ctx) = ctx_opt {
        close_webrtc_connections(&ctx).await;
        stop_webrtc_sweeper(&ctx);
        close_websocket(&ctx).await;
    }

    ENGINE.stop_background_loops();

    let mut state_lock = ENGINE.state.write().await;
    *state_lock = EngineState::Idle;
}

fn stop_webrtc_sweeper(ctx: &crate::engine::RunningContext) {
    if let Some(wt) = ctx.webrtc_transport.as_ref() {
        wt.stop_session_sweeper();
    }
}

async fn close_webrtc_connections(ctx: &crate::engine::RunningContext) {
    if let Some(wt) = ctx.webrtc_transport.as_ref() {
        wt.close_all_peer_connections().await;
    }
}

async fn close_websocket(ctx: &crate::engine::RunningContext) {
    if let Some(ws) = ctx.websocket_signaler.as_ref() {
        if let Err(err) = ws.close().await {
            tracing::warn!("leave_room: websocket close failed: {:?}", err);
        }
    }
}
