use crate::engine::{SessionCtx, ENGINE};
use mistlib_core::signaling::Signaler;
use std::sync::atomic::Ordering;

use super::init::build_session_context;

pub(super) fn join_room(room_id: String) {
    if !ENGINE.initialized.load(Ordering::Relaxed) {
        tracing::debug!("join_room({room_id}) ignored: init() has not been called yet");
        return;
    }

    ENGINE.runtime.spawn(async move {
        if ENGINE.has_session(&room_id).await {
            // Already active: keep the existing session and just re-announce
            // instead of rebuilding it (SPEC-15 rule 3).
            reannounce(&room_id).await;
            return;
        }

        let local_id = ENGINE.self_id.lock().unwrap().clone();
        let ctx = build_session_context(room_id.clone(), local_id).await;

        if !ENGINE.insert_session(room_id.clone(), ctx.clone()).await {
            // Lost a race with a concurrent join_room(room_id) call: another
            // session already exists for this room now, so fall back to
            // re-announcing through it instead of running two stacks for the
            // same room. `ctx` (the one we just built but never started) is
            // simply dropped here.
            reannounce(&room_id).await;
            return;
        }

        ENGINE.spawn_background_loops(ctx.clone());
        if let Err(e) = ENGINE.run(ctx).await {
            tracing::error!("Engine run error: {}", e);
        }
    });
}

async fn reannounce(room_id: &str) {
    if let Some(ctx) = ENGINE.get_session(room_id).await {
        if let Some(wt) = ctx.webrtc_transport.as_ref() {
            if let Err(err) = wt.announce_to_room().await {
                tracing::warn!("join_room: re-announce failed for {room_id}: {:?}", err);
            }
        }
    }
}

/// Tears down every active session (identical to today when only one room is
/// joined). Signature unchanged from the single-room API.
pub(super) fn leave_room() {
    if tokio::runtime::Handle::try_current().is_ok() {
        ENGINE.runtime.spawn(shutdown_all_sessions());
    } else {
        ENGINE.runtime.block_on(shutdown_all_sessions());
    }
}

/// Tears down only `room_id`'s session, leaving every other active room
/// untouched. Not-joined is a no-op (consistent with `join_room`'s own
/// silent-no-op style).
pub(super) fn leave_room_id(room_id: String) {
    if tokio::runtime::Handle::try_current().is_ok() {
        ENGINE.runtime.spawn(shutdown_session(room_id));
    } else {
        ENGINE.runtime.block_on(shutdown_session(room_id));
    }
}

async fn shutdown_all_sessions() {
    for (_, ctx) in ENGINE.remove_all_sessions().await {
        teardown(&ctx).await;
    }
}

async fn shutdown_session(room_id: String) {
    if let Some(ctx) = ENGINE.remove_session(&room_id).await {
        teardown(&ctx).await;
    } else {
        tracing::debug!("leave_room_id({room_id}) ignored: room is not joined");
    }
}

async fn teardown(ctx: &SessionCtx) {
    // Stops this session's background loops, network event pump, and
    // signaling loop -- and only this session's; every other active room's
    // cancel token is untouched.
    ctx.cancel.cancel();

    if let Some(wt) = ctx.webrtc_transport.as_ref() {
        wt.close_all_peer_connections().await;
        wt.stop_session_sweeper();
    }
    if let Some(signaler) = ctx.bootstrap_signaler.as_ref() {
        if let Err(err) = signaler.close().await {
            tracing::warn!(
                "leave_room: bootstrap signaling close failed for {}: {:?}",
                ctx.room_id,
                err
            );
        }
    }
}
