use crate::signaling::SignalingEnvelope;
use crate::types::{DeliveryMethod, NodeId};
use bytes::Bytes;

#[derive(Debug, Clone)]
pub enum OverlayAction {

    SendMessage {
        to: NodeId,
        data: Bytes,
        method: DeliveryMethod,
    },

    Connect { to: NodeId },

    Disconnect { to: NodeId },

    SendSignaling {
        to: NodeId,
        envelope: SignalingEnvelope,
    },
}
