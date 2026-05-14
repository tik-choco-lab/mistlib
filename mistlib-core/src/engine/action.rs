use super::{EngineState, MistEngine};
use crate::action::OverlayAction;
use crate::overlay::ActionHandler;

impl ActionHandler for MistEngine {
    fn handle_action(&self, action: OverlayAction) {
        let state_arc = self.state.clone();

        self.runtime.spawn(Box::pin(async move {
            let ctx = {
                let lock = state_arc.lock().expect("state lock poisoned");
                if let EngineState::Running(c) = &*lock {
                    Some(c.clone())
                } else {
                    None
                }
            };
            let Some(ctx) = ctx else { return };

            match action {
                OverlayAction::SendMessage { to, data, method } => {
                    let transport = ctx.preferred_transport();
                    let result = if to.is_broadcast() {
                        transport.broadcast(data, method).await
                    } else {
                        transport.send(&to, data, method).await
                    };
                    if let Err(e) = result {
                        tracing::warn!("Failed to send message to {}: {:?}", to.0, e);
                    }
                }
                OverlayAction::Connect { to } => {
                    if let Err(e) = ctx.preferred_transport().connect(&to).await {
                        tracing::warn!("Failed to connect to {}: {:?}", to.0, e);
                    }
                }
                OverlayAction::Disconnect { to } => {
                    if let Err(e) = ctx.preferred_transport().disconnect(&to).await {
                        tracing::warn!("Failed to disconnect from {}: {:?}", to.0, e);
                    }
                }
                OverlayAction::SendSignaling { to, envelope } => {
                    if let Some(sig) = &ctx.signaling_dispatch {
                        if let Err(e) = sig.send_signaling(&to, envelope.content).await {
                            tracing::warn!("Failed to send signaling to {}: {:?}", to.0, e);
                        }
                    }
                }
            }
        }));
    }
}
