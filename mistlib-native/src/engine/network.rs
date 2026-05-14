use std::sync::Arc;

use mistlib_core::overlay::ActionHandler;
use mistlib_core::signaling::MessageContent;
use mistlib_core::transport::NetworkEvent;
use tokio::sync::mpsc;

use super::{EngineState, RunningContext};

impl super::MistEngine {
    pub(super) async fn process_network_events(&self, mut rx: mpsc::Receiver<NetworkEvent>) {
        while let Some(event) = rx.recv().await {
            let ctx = {
                let state_lock = self.state.read().await;
                match &*state_lock {
                    EngineState::Running(ctx) => ctx.clone(),
                    _ => continue,
                }
            };

            let Some(ov) = ctx.overlay.as_ref() else {
                continue;
            };

            let from_origin = event.from.clone();
            match bincode::deserialize::<mistlib_core::overlay::OverlayEnvelope>(&event.data) {
                Ok(envelope) => {
                    self.handle_overlay_envelope(envelope, from_origin, &ctx, ov)
                        .await;
                }
                Err(e) => {
                    tracing::trace!("process_network_events: bincode deserialize failed ({e}), trying storage protocol");
                    self.handle_storage_message(&event.data, from_origin);
                }
            }
        }
    }

    async fn handle_overlay_envelope(
        &self,
        envelope: mistlib_core::overlay::OverlayEnvelope,
        from_origin: mistlib_core::types::NodeId,
        ctx: &Arc<RunningContext>,
        ov: &Arc<mistlib_core::overlay::OverlayRouter>,
    ) {
        ov.learn_route(&envelope.from, &from_origin);
        self.touch_node(&from_origin);
        self.touch_node(&envelope.from);

        let to_self = envelope.to == *self.self_id.lock().unwrap() || envelope.to.0.is_empty();
        let content = envelope.content.clone();

        let actions = ov.handle_envelope(envelope, from_origin.clone());
        for action in actions {
            self.handle_action(action);
        }

        if to_self {
            self.dispatch_local_message(content, from_origin, ctx);
        }
    }

    fn dispatch_local_message(
        &self,
        content: MessageContent,
        from_origin: mistlib_core::types::NodeId,
        ctx: &Arc<RunningContext>,
    ) {
        match content {
            MessageContent::Raw(payload) => {
                super::dispatch_event(super::EVENT_RAW, &from_origin, &payload);
            }
            MessageContent::Overlay(overlay_msg) => {
                if overlay_msg.is_internal_control() {
                    return;
                }
                super::dispatch_event(super::EVENT_OVERLAY, &from_origin, &overlay_msg.payload);
            }
            MessageContent::Data(signaling_data) => {
                self.ensure_node_registered(&signaling_data.sender_id);
                if let Some(handler) = ctx.p2p_signaling_handler.clone() {
                    self.runtime.handle().spawn(async move {
                        if let Err(err) = handler
                            .handle_message(MessageContent::Data(signaling_data))
                            .await
                        {
                            tracing::warn!("NativeEngine: p2p signaling handler failed: {:?}", err);
                        }
                    });
                } else {
                    tracing::debug!(
                        "NativeEngine: p2p signaling relay not configured, dropping message"
                    );
                }
            }
        }
    }

    fn handle_storage_message(&self, data: &[u8], from_origin: mistlib_core::types::NodeId) {
        use crate::storage::resolver;

        if let Some(cid) = resolver::parse_want_message(data) {
            let from = from_origin.clone();
            self.runtime.handle().spawn(async move {
                crate::storage::handle_want(from, cid).await;
            });
        } else if let Some(cid) = resolver::parse_query_message(data) {
            let from = from_origin.clone();
            self.runtime.handle().spawn(async move {
                crate::storage::handle_query(from, cid).await;
            });
        } else if let Some(cid) = resolver::parse_have_status_message(data) {
            crate::storage::handle_have_status(from_origin, cid);
        } else if let Some((cid, data)) = resolver::parse_have_message(data) {
            crate::storage::handle_have(cid, data);
        }
    }
}
