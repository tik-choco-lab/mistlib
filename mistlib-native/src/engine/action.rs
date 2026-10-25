use std::sync::Arc;

use mistlib_core::action::OverlayAction;
use mistlib_core::transport::Transport;

use super::{SessionCtx, ENGINE};

impl super::MistEngine {
    /// Synchronously admit a send to this session's ordered peer queue.
    /// The router calls this while holding its sequence lock, so concurrent
    /// app and overlay senders cannot invert stamp/enqueue order. Admission
    /// failures propagate before the router consumes the sequence number.
    pub(crate) fn try_dispatch_send_for(
        &self,
        ctx: Arc<SessionCtx>,
        action: OverlayAction,
    ) -> mistlib_core::error::Result<()> {
        if let OverlayAction::SendMessage {
            to, data, method, ..
        } = action
        {
            let wt = ctx.webrtc_transport.as_ref().ok_or_else(|| {
                mistlib_core::error::MistError::Internal("WebRTC transport is not available".into())
            })?;
            if to.is_broadcast() {
                wt.try_enqueue_broadcast(data, method)
            } else {
                wt.try_enqueue_send(&to, data, method)
            }
        } else {
            self.handle_action_for(ctx, action);
            Ok(())
        }
    }

    /// Retain admitted actions in call order while waiting for egress capacity.
    /// Newly originated sequenced sends instead use `try_dispatch_send` inside
    /// the router's sequence critical section, before consuming a sequence.
    pub(crate) fn handle_action_for(&self, ctx: Arc<SessionCtx>, action: OverlayAction) {
        if let OverlayAction::SendMessage {
            to, data, method, ..
        } = action
        {
            if let Some(wt) = &ctx.webrtc_transport {
                let send = wt.retain_received_send(&to, data, method);
                self.runtime.handle().spawn(async move {
                    if let Err(err) = send.await {
                        tracing::warn!("Failed to send overlay action: {err:?}");
                    }
                });
            }
            return;
        }

        let handle = self.runtime.handle().clone();
        handle.spawn(async move {
            match action {
                OverlayAction::SendMessage { .. } => unreachable!(
                    "handled synchronously above and returned before reaching this spawn"
                ),
                OverlayAction::Connect { to } => {
                    if let Some(wt) = &ctx.webrtc_transport {
                        if let Err(err) = wt.connect(&to).await {
                            tracing::warn!("Failed to connect overlay action to {}: {err:?}", to.0);
                        }
                    }
                }
                OverlayAction::Disconnect { to } => {
                    if let Some(wt) = &ctx.webrtc_transport {
                        if let Err(err) = wt.disconnect(&to).await {
                            tracing::warn!(
                                "Failed to disconnect overlay action to {}: {err:?}",
                                to.0
                            );
                        }
                    }
                }
                OverlayAction::SuspectDisconnected { to } => {
                    if let Some(wt) = &ctx.webrtc_transport {
                        if let Err(err) = wt.suspect_disconnected(&to).await {
                            tracing::warn!(
                                "Failed to mark suspect-disconnected overlay action to {}: {err:?}",
                                to.0
                            );
                        }
                    }
                }
                OverlayAction::ClearSuspect { to } => {
                    if let Some(wt) = &ctx.webrtc_transport {
                        if let Err(err) = wt.clear_suspect(&to).await {
                            tracing::warn!(
                                "Failed to clear-suspect overlay action to {}: {err:?}",
                                to.0
                            );
                        }
                    }
                }
                OverlayAction::SendSignaling { to, envelope } => {
                    if let Some(sig) = &ctx.signaling_dispatch {
                        if let Err(err) = sig.send_signaling(&to, envelope.content).await {
                            tracing::warn!(
                                "Failed to send signaling overlay action to {}: {err:?}",
                                to.0
                            );
                        }
                    }
                }
            }
        });
    }

    /// Resolve the session synchronously so sequenced sends reach its queue
    /// without leaving the router's sequence critical section.
    pub(crate) fn try_dispatch_send_in_room(
        &self,
        room_id: &str,
        action: OverlayAction,
    ) -> mistlib_core::error::Result<()> {
        let ctx = self.get_session_sync(room_id).ok_or_else(|| {
            mistlib_core::error::MistError::Internal(format!("No active session: {room_id}"))
        })?;
        self.try_dispatch_send_for(ctx, action)
    }

    /// Handle general actions through the session bound to this handler.
    pub(crate) fn handle_action_in_room(&self, room_id: String, action: OverlayAction) {
        if let Some(ctx) = ENGINE.get_session_sync(&room_id) {
            ENGINE.handle_action_for(ctx, action);
        }
    }
}
