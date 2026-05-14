pub mod message;
pub mod relay;
pub mod r#trait;

pub use message::*;
pub use r#trait::{Signaler, SignalingHandler};
pub use relay::{RoutedSignaler, RoutedSignalingHandler, SignalingRoute};

// Backward-compatible re-exports. Prefer `crate::overlay::*` for overlay protocol
// messages; these do not belong to signaling conceptually.
#[deprecated(note = "use crate::overlay::OverlayEnvelope")]
pub use crate::overlay::OverlayEnvelope;
#[deprecated(note = "use crate::overlay::OverlayMessage")]
pub use crate::overlay::OverlayMessage;
#[deprecated(note = "use crate::overlay::OVERLAY_MSG_HEARTBEAT")]
pub use crate::overlay::OVERLAY_MSG_HEARTBEAT;
#[deprecated(note = "use crate::overlay::OVERLAY_MSG_NODE_LIST")]
pub use crate::overlay::OVERLAY_MSG_NODE_LIST;
#[deprecated(note = "use crate::overlay::OVERLAY_MSG_PING")]
pub use crate::overlay::OVERLAY_MSG_PING;
#[deprecated(note = "use crate::overlay::OVERLAY_MSG_PONG")]
pub use crate::overlay::OVERLAY_MSG_PONG;
#[deprecated(note = "use crate::overlay::OVERLAY_MSG_POSITION")]
pub use crate::overlay::OVERLAY_MSG_POSITION;
#[deprecated(note = "use crate::overlay::OVERLAY_MSG_REQUEST_NODE_LIST")]
pub use crate::overlay::OVERLAY_MSG_REQUEST_NODE_LIST;
