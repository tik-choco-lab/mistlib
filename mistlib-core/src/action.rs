use crate::overlay::OverlayEnvelope;
use crate::types::{DeliveryMethod, NodeId};
use bytes::Bytes;

/// Local scheduling metadata; never serialized into an overlay envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SendLane {
    #[default]
    General,
    Control,
    Bulk,
}

impl SendLane {
    pub fn classify(content: &crate::signaling::MessageContent, broadcast: bool) -> Self {
        use crate::signaling::MessageContent;
        use crate::storage::protocol::{classify_storage_frame, StorageFrame};
        match content {
            MessageContent::Raw(data) => match classify_storage_frame(data) {
                Some(StorageFrame::HaveChunk { .. }) if !broadcast => Self::Bulk,
                Some(StorageFrame::HaveChunk { .. }) | None => Self::General,
                Some(_) => Self::Control,
            },
            MessageContent::Overlay(_) | MessageContent::Data(_) => Self::Control,
        }
    }
}

#[derive(Debug, Clone)]
pub enum OverlayAction {
    SendMessage {
        to: NodeId,
        data: Bytes,
        method: DeliveryMethod,
        lane: SendLane,
    },

    Connect {
        to: NodeId,
    },

    Disconnect {
        to: NodeId,
    },

    /// PING/PONG liveness has just crossed the configured miss threshold for
    /// this peer. The transport is expected to fold this into the same
    /// reconnect-grace flow used for its own disconnect signals (e.g. ICE
    /// Disconnected), so a genuine loss gets cleaned up promptly. Emitted at
    /// most once per suspect episode; see [`OverlayAction::ClearSuspect`].
    SuspectDisconnected {
        to: NodeId,
    },

    /// A PONG arrived from a peer that was previously latched as
    /// [`OverlayAction::SuspectDisconnected`], so the transport should cancel
    /// that suspicion -- but only if the grace period it is currently in was
    /// started by the suspicion itself. A grace period started by a
    /// transport-level signal (e.g. ICE Disconnected) must be left alone;
    /// that one only ends via the transport's own recovery signal.
    ClearSuspect {
        to: NodeId,
    },

    SendSignaling {
        to: NodeId,
        envelope: OverlayEnvelope,
    },
}
