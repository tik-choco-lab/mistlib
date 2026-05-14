use mistlib_core::action::OverlayAction;
use mistlib_core::overlay::ActionHandler;
use mistlib_core::transport::Transport;
use mistlib_core::types::NodeId;

use super::{EngineState, ENGINE};

impl ActionHandler for super::MistEngine {
    fn handle_action(&self, action: OverlayAction) {
        let handle = self.runtime.handle().clone();
        handle.spawn(async move {
            let state_lock = ENGINE.state.read().await;
            if let EngineState::Running(ctx) = &*state_lock {
                match action {
                    OverlayAction::SendMessage { to, data, method } => {
                        if let Some(wt) = &ctx.webrtc_transport {
                            let result = if to.is_broadcast() {
                                wt.broadcast(data, method).await
                            } else {
                                wt.send(&to, data, method).await
                            };
                            if let Err(err) = result {
                                let target = if to.is_broadcast() {
                                    NodeId::BROADCAST
                                } else {
                                    &to.0
                                };
                                tracing::warn!(
                                    "Failed to send overlay action to {target}: {err:?}"
                                );
                            }
                        }
                    }
                    OverlayAction::Connect { to } => {
                        if let Some(wt) = &ctx.webrtc_transport {
                            let _ = wt.connect(&to).await;
                        }
                    }
                    OverlayAction::Disconnect { to } => {
                        if let Some(wt) = &ctx.webrtc_transport {
                            let _ = wt.disconnect(&to).await;
                        }
                    }
                    OverlayAction::SendSignaling { to, envelope } => {
                        if let Some(sig) = &ctx.signaling_dispatch {
                            let _ = sig.send_signaling(&to, envelope.content).await;
                        }
                    }
                }
            }
        });
    }
}
