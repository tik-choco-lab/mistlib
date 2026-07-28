use std::sync::Arc;

use mistlib_core::action::OverlayAction;
use mistlib_core::transport::Transport;
use mistlib_core::types::NodeId;

use super::{SessionCtx, ENGINE};

impl super::MistEngine {
    /// Applies a single overlay-generated action against `ctx`'s session
    /// stack (WebRTC transport, signaling dispatch).
    ///
    /// `SendMessage` is handled inline, synchronously -- NOT spawned. Overlay
    /// seq numbers are stamped synchronously, in call order, before this
    /// function is ever reached (`OverlayRouter::wrap_data`, called from
    /// `OverlayTransport::send`/`broadcast`); spawning a fresh task per send
    /// here would hand that ordering to `tokio::spawn`, which makes no
    /// guarantee that tasks run in the order they were spawned in (confirmed
    /// by direct reproduction: N concurrently-spawned senders to the same
    /// peer reliably wrote to the DataChannel out of order). Since
    /// `WebRtcTransport::try_enqueue_send`/`try_enqueue_broadcast` are fully
    /// synchronous (no `.await`, never block -- see their doc comments),
    /// calling them here directly costs nothing worth spawning for, and it's
    /// what actually makes `Peer::spawn_send_queue`'s per-peer ordering
    /// guarantee hold: that queue only preserves *enqueue* order, so the
    /// enqueue call itself must happen in the caller's true order.
    ///
    /// Every other action type is still spawned fire-and-forget, as before,
    /// so a slow `connect`/`disconnect`/signaling round trip never blocks the
    /// caller (the per-session background tick loop, the network event pump,
    /// `notify_peer_disconnected`, or -- now -- `handle_action_in_room`,
    /// which itself no longer spawns for the same reason described above).
    pub(crate) fn handle_action_for(&self, ctx: Arc<SessionCtx>, action: OverlayAction) {
        if let OverlayAction::SendMessage { to, data, method } = &action {
            if let Some(wt) = &ctx.webrtc_transport {
                let result = if to.is_broadcast() {
                    wt.try_enqueue_broadcast(data.clone(), *method);
                    Ok(())
                } else {
                    wt.try_enqueue_send(to, data.clone(), *method)
                };
                if let Err(err) = result {
                    let target = if to.is_broadcast() {
                        NodeId::BROADCAST
                    } else {
                        &to.0
                    };
                    tracing::warn!("Failed to send overlay action to {target}: {err:?}");
                }
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

    /// Same as `handle_action_for`, but resolves `room_id` to its session
    /// first. This is what each session's `ActionHandler` (wired into its
    /// own `OverlayRouter` at construction time -- see
    /// `layers/native_l0/init.rs`) actually calls: mistlib-core's
    /// `ActionHandler` trait has no room parameter, so the handler instance
    /// itself is what carries the room_id.
    ///
    /// Resolves the session synchronously (`get_session_sync`, backed by a
    /// `std::sync::RwLock` -- see `MistEngine::sessions`'s doc comment) and
    /// calls `handle_action_for` inline instead of spawning a task for the
    /// lookup: this is `SessionActionHandler::handle_action`'s (the real
    /// `ActionHandler` wired into every session's `OverlayRouter`) only path
    /// down to `handle_action_for`, so if *this* spawned independently per
    /// call, `handle_action_for`'s own synchronous, in-order handling of
    /// `SendMessage` would be undermined one layer up -- N concurrently
    /// spawned session lookups have no guaranteed relative order either.
    pub(crate) fn handle_action_in_room(&self, room_id: String, action: OverlayAction) {
        if let Some(ctx) = ENGINE.get_session_sync(&room_id) {
            ENGINE.handle_action_for(ctx, action);
        }
    }
}
