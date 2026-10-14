use super::OverlayRouter;
use crate::action::OverlayAction;
use crate::overlay::OverlayEnvelope;
use crate::signaling::MessageContent;
use crate::types::{DeliveryMethod, NodeId};
use bytes::Bytes;

impl OverlayRouter {
    fn create_send_action(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> OverlayAction {
        let rt = self
            .routing_table
            .lock()
            .expect("routing_table lock poisoned");
        let next_hop = rt.get_next_hop(node).unwrap_or_else(|| node.clone());
        OverlayAction::SendMessage {
            to: next_hop,
            data,
            method,
        }
    }

    pub fn wrap_data(&self, to: &NodeId, data: Bytes, method: DeliveryMethod) -> OverlayAction {
        self.wrap_data_checked(to, data, method, |_| Ok(()))
            .expect("OverlayEnvelope serialization must not fail")
    }

    /// Validate the actual encoded frame before committing a sequence or outgoing state.
    /// The validator runs while the destination sequence is reserved; it must not
    /// re-enter this router's send methods.
    pub fn wrap_data_checked(
        &self,
        to: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
        validate: impl FnOnce(usize) -> crate::error::Result<()>,
    ) -> crate::error::Result<OverlayAction> {
        let build = |seq| {
            let envelope = OverlayEnvelope::new(
                self.local_node_id.clone(),
                to.clone(),
                self.hop_count,
                MessageContent::Raw(data),
            )
            .with_seq(seq);
            let encoded = crate::overlay::wire::serialize(&envelope)
                .map_err(|err| crate::error::MistError::Internal(err.to_string()))?;
            validate(encoded.len())?;
            self.remember_outgoing(&envelope);
            Ok(self.create_send_action(to, Bytes::from(encoded), method))
        };
        if method == DeliveryMethod::ReliableOrdered && !to.is_broadcast() {
            self.with_next_seq(to, build)
        } else {
            build(0)
        }
    }
}
