use super::{EngineEvent, MistEngine};
use crate::overlay::dnve3::Vector3;
use crate::overlay::ActionHandler;
use crate::overlay::OverlayEnvelope;
use crate::signaling::MessageContent;
use crate::transport::NetworkEvent;
use crate::types::NodeId;

const MSG_TYPE_SYNC_POS: u32 = 100;
/// Binary position sync: payload is a bincode `Vector3` (12 bytes). Senders:
/// `mistlib-wasm/src/app.rs::MSG_TYPE_POSITION_SYNC_BIN` (must stay in sync).
const MSG_TYPE_SYNC_POS_BIN: u32 = 200;

impl MistEngine {
    pub(super) async fn process_network_event(&self, event: NetworkEvent) {
        let from = event.from.clone();
        tracing::debug!(
            "MistEngine: Received network event from {}, len={}",
            from.0,
            event.data.len()
        );

        let Ok(envelope) = crate::overlay::wire::deserialize::<OverlayEnvelope>(&event.data) else {
            tracing::debug!(
                "MistEngine: Deserialization failed for event from {}. Forwarding as raw data.",
                from.0
            );
            self.touch_node(&from);
            self.emit(EngineEvent::RawMessage(from, event.data));
            return;
        };

        let envelope_from = envelope.from.clone();
        if let Some(ov) = self.running_context().and_then(|ctx| ctx.overlay.clone()) {
            ov.learn_route(&envelope_from, &from);
        }
        self.touch_node(&from);
        self.touch_node(&envelope_from);

        let to_self = {
            let id = self.self_id.lock().expect("self_id lock poisoned");
            envelope.to == *id || envelope.to.is_broadcast()
        };

        tracing::debug!(
            "MistEngine: Envelope from={}, to={}, to_self={}",
            envelope.from.0,
            envelope.to.0,
            to_self
        );

        let content = envelope.content.clone();
        let seq = envelope.seq;

        let overlay = self.running_context().and_then(|ctx| ctx.overlay.clone());
        let result = overlay
            .as_ref()
            .map(|ov| ov.handle_envelope(envelope, from.clone()));

        let should_deliver = result
            .as_ref()
            .map(|result| result.should_deliver)
            .unwrap_or(true);

        if let Some(result) = result {
            for action in result.actions {
                self.handle_action(action);
            }
        }

        if to_self && should_deliver {
            // Restore end-to-end order per source before dispatching. seq == 0 and
            // the no-overlay path deliver immediately (no reordering applied).
            match overlay {
                Some(ov) => {
                    for ordered in ov.reorder_inbound(&envelope_from, seq, content) {
                        self.handle_message_content(envelope_from.clone(), ordered);
                    }
                }
                None => self.handle_message_content(envelope_from, content),
            }
        }
    }

    /// Dispatches a decoded message to the appropriate handler based on content type.
    ///
    /// `pub(super)` (not private): also called from `tick.rs`'s periodic
    /// `flush_expired_inbound` poll, which delivers reorder-buffer gaps that
    /// timed out without new traffic to re-trigger the lazy flush above.
    pub(super) fn handle_message_content(&self, from: NodeId, content: MessageContent) {
        match content {
            MessageContent::Raw(payload) => {
                tracing::debug!(
                    "MistEngine: Dispatching RawMessage, payload_len={}",
                    payload.len()
                );
                self.emit(EngineEvent::RawMessage(from, payload));
            }
            MessageContent::Overlay(msg) => {
                tracing::debug!(
                    "MistEngine: Dispatching OverlayMessage type={}",
                    msg.message_type
                );
                if msg.is_internal_control() {
                    return;
                }
                if msg.message_type == MSG_TYPE_SYNC_POS {
                    self.try_apply_sync_pos(&from, &msg.payload);
                }
                if msg.message_type == MSG_TYPE_SYNC_POS_BIN {
                    self.try_apply_sync_pos_bin(&from, &msg.payload);
                }
                self.emit(EngineEvent::OverlayMessage(from, msg.payload));
            }
            MessageContent::Data(sig_data) => {
                tracing::debug!(
                    "MistEngine: Handling signaling Data from {} for {}",
                    sig_data.sender_id.0,
                    sig_data.receiver_id.0
                );
                self.ensure_node_registered(&sig_data.sender_id);
                if let Some(c) = self.running_context() {
                    let handler = c
                        .p2p_signaling_handler
                        .clone()
                        .unwrap_or_else(|| c.signaling_handler.clone());
                    self.runtime.spawn(Box::pin(async move {
                        if let Err(err) =
                            handler.handle_message(MessageContent::Data(sig_data)).await
                        {
                            tracing::warn!(
                                "MistEngine: signaling message dispatch failed: {:?}",
                                err
                            );
                        }
                    }));
                }
            }
        }
    }

    /// Registers a node at the origin if it is not yet known and is not the server.
    fn ensure_node_registered(&self, node_id: &NodeId) {
        if node_id.is_server() {
            return;
        }
        let mut store = self.node_store.lock().expect("node_store lock poisoned");
        if !store.nodes.contains_key(node_id) {
            store.update_node_position(node_id.clone(), Vector3::new(0.0, 0.0, 0.0));
        }
    }

    /// Marks a known node as active without changing its last known position.
    fn touch_node(&self, node_id: &NodeId) {
        if node_id.is_server() {
            return;
        }
        let mut store = self.node_store.lock().expect("node_store lock poisoned");
        store.touch_node(node_id);
    }

    /// Parses a `sync_pos` payload and updates the node's position in the store.
    fn try_apply_sync_pos(&self, from: &NodeId, payload: &[u8]) {
        let Ok(data) = serde_json::from_slice::<serde_json::Value>(payload) else {
            return;
        };
        if data["type"] != "sync_pos" {
            return;
        }
        let (Some(x), Some(y), Some(z)) = (
            data["position"]["x"].as_f64(),
            data["position"]["y"].as_f64(),
            data["position"]["z"].as_f64(),
        ) else {
            return;
        };
        tracing::debug!(
            "MistEngine: Updating node {} position to ({}, {}, {})",
            from.0,
            x,
            y,
            z
        );
        self.node_store
            .lock()
            .expect("node_store lock poisoned")
            .update_node_position(from.clone(), Vector3::new(x as f32, y as f32, z as f32));
    }

    /// Applies a binary position-sync payload (bincode `Vector3`) to the node store.
    fn try_apply_sync_pos_bin(&self, from: &NodeId, payload: &[u8]) {
        // Strict length check so unrelated overlay payloads can never be
        // misread as a position (bincode tolerates trailing bytes).
        if payload.len() != std::mem::size_of::<f32>() * 3 {
            return;
        }
        let Ok(pos) = bincode::deserialize::<Vector3>(payload) else {
            return;
        };
        self.node_store
            .lock()
            .expect("node_store lock poisoned")
            .update_node_position(from.clone(), pos);
    }
}
