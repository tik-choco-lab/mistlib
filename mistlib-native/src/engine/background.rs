use mistlib_core::overlay::ActionHandler;
use tokio_util::sync::CancellationToken;

use super::{EngineState, ENGINE};

impl super::MistEngine {
    pub fn spawn_background_loops(&self) {
        let cancel = CancellationToken::new();
        {
            let mut lock = self.background_loops_cancel.lock().unwrap();
            if lock.is_some() {
                return;
            }
            *lock = Some(cancel.clone());
        }

        let cancel_aoi = cancel.clone();
        self.runtime.spawn(async move {
            let mut interval = tokio::time::interval(web_time::Duration::from_millis(1000));
            loop {
                tokio::select! {
                    _ = cancel_aoi.cancelled() => break,
                    _ = interval.tick() => {}
                }
                ENGINE.check_and_dispatch_aoi().await;
                ENGINE.check_and_dispatch_neighbors().await;
            }
        });

        let cancel_overlay = cancel;
        self.runtime.spawn(async move {
            let mut interval = tokio::time::interval(web_time::Duration::from_millis(1000));
            loop {
                tokio::select! {
                    _ = cancel_overlay.cancelled() => break,
                    _ = interval.tick() => {}
                }

                let actions = {
                    let state_lock = ENGINE.state.read().await;
                    match &*state_lock {
                        EngineState::Running(ctx) => {
                            if let (Some(ov), Some(wt)) = (&ctx.overlay, &ctx.webrtc_transport) {
                                let states = wt.get_active_connection_states();
                                ov.sync_connection_states(&states);
                                let config = ENGINE.config.lock().unwrap().clone();
                                ov.tick(&config, &states)
                            } else {
                                vec![]
                            }
                        }
                        _ => vec![],
                    }
                };

                for action in actions {
                    ENGINE.handle_action(action);
                }
            }
        });
    }

    pub fn stop_background_loops(&self) {
        let cancel = self.background_loops_cancel.lock().unwrap().take();
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }
}
