use crate::action::OverlayAction;
use crate::overlay::{OverlayEnvelope, OverlayMessage, OVERLAY_MSG_PING, OVERLAY_MSG_PONG};
use crate::signaling::MessageContent;
use crate::stats::STATS;
use crate::types::{DeliveryMethod, NodeId};
use std::sync::OnceLock;
use web_time::Instant;

fn monotonic_micros() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_micros() as u64
}

fn send_ping_all(
    local_node_id: &NodeId,
    hop_count: u32,
    connected_nodes: &[NodeId],
) -> Vec<OverlayAction> {
    let now = monotonic_micros();
    let payload = now.to_le_bytes().to_vec();

    connected_nodes
        .iter()
        .filter_map(|target| {
            let envelope = OverlayEnvelope {
                from: local_node_id.clone(),
                to: target.clone(),
                hop_count,
                content: MessageContent::Overlay(OverlayMessage {
                    message_type: OVERLAY_MSG_PING,
                    payload: payload.clone(),
                }),
            };

            let data = bincode::serialize(&envelope)
                .map_err(|e| {
                    tracing::warn!("[Stats] failed to serialize ping to {}: {}", target, e)
                })
                .ok()?;
            Some(OverlayAction::SendMessage {
                to: target.clone(),
                data: bytes::Bytes::from(data),
                method: DeliveryMethod::Unreliable,
            })
        })
        .collect()
}

pub(crate) fn tick_actions(
    local_node_id: &NodeId,
    hop_count: u32,
    connected_nodes: &[NodeId],
) -> Vec<OverlayAction> {
    send_ping_all(local_node_id, hop_count, connected_nodes)
}

pub(crate) fn handle_ping(
    local_node_id: &NodeId,
    from: NodeId,
    hop_count: u32,
    payload: &[u8],
) -> Vec<OverlayAction> {
    let envelope = OverlayEnvelope {
        from: local_node_id.clone(),
        to: from.clone(),
        hop_count,
        content: MessageContent::Overlay(OverlayMessage {
            message_type: OVERLAY_MSG_PONG,
            payload: payload.to_vec(),
        }),
    };

    match bincode::serialize(&envelope) {
        Ok(data) => vec![OverlayAction::SendMessage {
            to: from,
            data: bytes::Bytes::from(data),
            method: DeliveryMethod::Unreliable,
        }],
        Err(e) => {
            tracing::warn!("[Stats] failed to serialize pong envelope: {}", e);
            vec![]
        }
    }
}

pub(crate) fn handle_pong(from: NodeId, payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }
    let sent_time = u64::from_le_bytes(payload[..8].try_into().unwrap_or_default());
    let now = monotonic_micros();
    let rtt_ms = now.saturating_sub(sent_time) as f32 / 1000.0;
    STATS.set_rtt(from, rtt_ms);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::OVERLAY_MSG_PONG;

    fn node(id: &str) -> NodeId {
        NodeId(id.to_string())
    }

    #[test]
    fn handle_ping_returns_pong_action() {
        let payload = 12345u64.to_le_bytes().to_vec();
        let actions = handle_ping(&node("local"), node("sender"), 1, &payload);
        assert_eq!(
            actions.len(),
            1,
            "handle_ping should return one pong action"
        );

        let OverlayAction::SendMessage { data, .. } = &actions[0] else {
            panic!("pong action should be SendMessage");
        };
        let envelope: OverlayEnvelope =
            bincode::deserialize(data).expect("pong data should be a valid envelope");
        let MessageContent::Overlay(msg) = &envelope.content else {
            panic!("pong message should be an overlay message");
        };
        assert_eq!(msg.message_type, OVERLAY_MSG_PONG);
        assert_eq!(&msg.payload, &payload, "pong payload should echo ping");
    }

    #[test]
    fn handle_pong_short_payload_is_noop() {
        handle_pong(node("peer"), b"short");
    }
}
