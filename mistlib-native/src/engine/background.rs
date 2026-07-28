use std::sync::Arc;

use super::{SessionCtx, ENGINE};

impl super::MistEngine {
    /// Spawns this session's periodic AOI/neighbor and overlay-tick loops.
    /// Called exactly once, right after a session is inserted into the
    /// registry (see `layers/native_l0/room.rs::join_room`); cancelled via
    /// `ctx.cancel` when the room is left.
    pub fn spawn_background_loops(&self, ctx: Arc<SessionCtx>) {
        let cancel_aoi = ctx.cancel.clone();
        let ctx_aoi = ctx.clone();
        self.runtime.spawn(async move {
            let mut interval = tokio::time::interval(web_time::Duration::from_millis(1000));
            loop {
                tokio::select! {
                    _ = cancel_aoi.cancelled() => break,
                    _ = interval.tick() => {}
                }
                ctx_aoi.check_and_dispatch_aoi().await;
                ctx_aoi.check_and_dispatch_neighbors().await;
            }
        });

        let cancel_overlay = ctx.cancel.clone();
        self.runtime.spawn(async move {
            let mut interval = tokio::time::interval(web_time::Duration::from_millis(1000));
            loop {
                tokio::select! {
                    _ = cancel_overlay.cancelled() => break,
                    _ = interval.tick() => {}
                }

                let actions = if let (Some(ov), Some(wt)) = (&ctx.overlay, &ctx.webrtc_transport) {
                    let states = wt.get_active_connection_states();
                    ov.sync_connection_states(&states);
                    let config = ENGINE.config.lock().unwrap().clone();
                    ov.tick(&config, &states)
                } else {
                    vec![]
                };

                for action in actions {
                    ENGINE.handle_action_for(ctx.clone(), action);
                }

                ENGINE.flush_expired_reorder(&ctx);
            }
        });
    }
}
