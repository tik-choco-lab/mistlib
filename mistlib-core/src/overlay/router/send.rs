use super::OverlayRouter;
use crate::action::{OverlayAction, SendLane};
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
        lane: SendLane,
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
            lane,
        }
    }

    /// Build a detached action; use `dispatch_data` for atomic send admission.
    pub fn wrap_data(&self, to: &NodeId, data: Bytes, method: DeliveryMethod) -> OverlayAction {
        self.wrap_data_checked(to, data, method, |_| Ok(()))
            .expect("OverlayEnvelope serialization must not fail")
    }

    /// Build a detached action. Actual senders should use `dispatch_data` so
    /// transport admission shares the sequence critical section.
    pub fn wrap_data_checked(
        &self,
        to: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
        validate: impl FnOnce(usize) -> crate::error::Result<()>,
    ) -> crate::error::Result<OverlayAction> {
        self.dispatch_data(to, data, method, validate, Ok)
    }

    /// Encode, validate, and hand off a frame before consuming its sequence.
    /// For reliable unicast, `commit` runs inside the destination's sequence
    /// critical section, so queue order equals sequence order across all senders.
    /// Only successfully committed frames are remembered as outgoing.
    ///
    /// Both callbacks must be synchronous, non-blocking, and must not re-enter
    /// this router. `commit` must return an error without handing off the frame
    /// if admission fails. Reserve any awaitable queue capacity beforehand.
    /// Broadcast and unreliable frames use sequence zero.
    pub fn dispatch_data<T>(
        &self,
        to: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
        validate: impl FnOnce(usize) -> crate::error::Result<()>,
        commit: impl FnOnce(OverlayAction) -> crate::error::Result<T>,
    ) -> crate::error::Result<T> {
        self.dispatch_data_via(to, None, data, method, validate, commit)
    }

    /// A reservation pins the next hop across the asynchronous capacity wait.
    pub fn dispatch_data_via<T>(
        &self,
        to: &NodeId,
        next_hop: Option<&NodeId>,
        data: Bytes,
        method: DeliveryMethod,
        validate: impl FnOnce(usize) -> crate::error::Result<()>,
        commit: impl FnOnce(OverlayAction) -> crate::error::Result<T>,
    ) -> crate::error::Result<T> {
        let lane = SendLane::classify(&MessageContent::Raw(data.clone()), to.is_broadcast());
        let build = |seq| {
            let method = crate::transport::send::storage_delivery(
                method,
                lane,
                seq,
                self.transfer.bulk_channel,
            );
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
            let action = if let Some(next_hop) = next_hop {
                OverlayAction::SendMessage {
                    to: next_hop.clone(),
                    data: Bytes::from(encoded),
                    method,
                    lane,
                }
            } else {
                self.create_send_action(to, Bytes::from(encoded), method, lane)
            };
            let result = commit(action)?;
            self.remember_outgoing(&envelope);
            Ok(result)
        };
        if method == DeliveryMethod::ReliableOrdered
            && !to.is_broadcast()
            && !(self.transfer.unsequenced_bulk && lane == SendLane::Bulk)
        {
            self.with_next_seq(to, build)
        } else {
            build(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::overlay::node_store::NodeStore;
    use crate::storage::protocol::{build_have_chunk_message, build_want_message};
    use std::sync::{Arc, Mutex};

    fn router(enabled: bool) -> OverlayRouter {
        let mut config = Config::default();
        config.storage.transfer.unsequenced_bulk = enabled;
        OverlayRouter::new(
            &config,
            Arc::new(Mutex::new(NodeStore::new())),
            NodeId("source".into()),
        )
    }

    fn unpack(action: OverlayAction) -> (OverlayEnvelope, SendLane) {
        let OverlayAction::SendMessage { data, lane, .. } = action else {
            panic!("expected send");
        };
        (crate::overlay::wire::deserialize(&data).unwrap(), lane)
    }

    #[test]
    fn only_valid_unicast_chunks_bypass_sequence_when_enabled() {
        let cid = crate::storage::cid::compute_cid(b"data", 0x55);
        let chunk = Bytes::from(build_have_chunk_message(&cid, 0, 1, b"data"));
        for enabled in [false, true] {
            let router = router(enabled);
            let peer = NodeId("destination".into());
            let send =
                |data| unpack(router.wrap_data(&peer, data, DeliveryMethod::ReliableOrdered));
            let (envelope, lane) = send(chunk.clone());
            assert_eq!(lane, SendLane::Bulk);
            assert_eq!(envelope.seq, if enabled { 0 } else { 1 });
            let (envelope, lane) = send(Bytes::from(build_want_message(&cid)));
            assert_eq!(lane, SendLane::Control);
            assert_eq!(envelope.seq, if enabled { 1 } else { 2 });
            let (envelope, lane) = send(Bytes::from_static(&[5, 0, 0, 0, 0, 0]));
            assert_eq!(lane, SendLane::General);
            assert_eq!(envelope.seq, if enabled { 2 } else { 3 });
            let (envelope, lane) = unpack(router.wrap_data(
                &NodeId::broadcast(),
                chunk.clone(),
                DeliveryMethod::ReliableOrdered,
            ));
            assert_eq!(envelope.seq, 0);
            assert_eq!(lane, SendLane::General);
        }
    }

    #[test]
    fn mixed_flag_relays_preserve_zero_and_positive_sequences() {
        let cid = crate::storage::cid::compute_cid(b"data", 0x55);
        for enabled in [false, true] {
            for seq in [0, 9] {
                let router = router(enabled);
                let envelope = OverlayEnvelope::new(
                    NodeId("origin".into()),
                    NodeId("destination".into()),
                    3,
                    MessageContent::Raw(Bytes::from(build_have_chunk_message(&cid, 0, 1, b"data"))),
                )
                .with_seq(seq);
                let msg_id = envelope.msg_id;
                let result = router.handle_envelope(envelope, NodeId("neighbor".into()));
                let (forwarded, lane) = unpack(
                    result
                        .actions
                        .into_iter()
                        .find(|a| matches!(a, OverlayAction::SendMessage { .. }))
                        .unwrap(),
                );
                assert_eq!(lane, SendLane::Bulk);
                assert_eq!(forwarded.seq, seq);
                assert_eq!(forwarded.msg_id, msg_id);
                assert_eq!(forwarded.hop_count, 2);
            }
        }
    }

    #[test]
    fn reliable_unordered_bypasses_reorder_and_does_not_consume_sequence() {
        let sender = router(false);
        let receiver = router(false);
        let peer = NodeId("destination".into());
        let ordered = |data| {
            unpack(sender.wrap_data(
                &peer,
                Bytes::from_static(data),
                DeliveryMethod::ReliableOrdered,
            ))
            .0
        };
        assert_eq!(ordered(b"one").seq, 1);
        let unordered = unpack(sender.wrap_data(
            &peer,
            Bytes::from_static(b"unordered"),
            DeliveryMethod::ReliableUnordered,
        ))
        .0;
        assert_eq!(unordered.seq, 0);
        assert_eq!(ordered(b"two").seq, 2);
        assert!(receiver
            .reorder_inbound(
                &unordered.from,
                3,
                MessageContent::Raw(Bytes::from_static(b"gap"))
            )
            .is_empty());
        assert_eq!(
            receiver
                .reorder_inbound(&unordered.from, unordered.seq, unordered.content)
                .len(),
            1
        );
    }

    #[test]
    fn reliable_unordered_non_storage_relays_use_ordered_and_preserve_sequence() {
        for seq in [0, 9] {
            let router = router(true);
            let envelope = OverlayEnvelope::new(
                NodeId("origin".into()),
                NodeId("destination".into()),
                3,
                MessageContent::Raw(Bytes::from_static(b"app ping")),
            )
            .with_seq(seq);
            let result = router.handle_envelope(envelope, NodeId("neighbor".into()));
            let action = result
                .actions
                .into_iter()
                .find(|a| matches!(a, OverlayAction::SendMessage { .. }))
                .unwrap();
            assert!(matches!(
                &action,
                OverlayAction::SendMessage {
                    method: DeliveryMethod::ReliableOrdered,
                    ..
                }
            ));
            assert_eq!(unpack(action).0.seq, seq);
        }
    }

    #[test]
    fn storage_method_requires_both_flags_at_source_and_zero_sequence_at_relay() {
        let cid = crate::storage::cid::compute_cid(b"data", 0x55);
        let chunk = Bytes::from(build_have_chunk_message(&cid, 0, 1, b"data"));
        for unsequenced in [false, true] {
            for bulk_channel in [false, true] {
                let mut config = Config::default();
                config.storage.transfer.unsequenced_bulk = unsequenced;
                config.storage.transfer.bulk_channel = bulk_channel;
                let router = OverlayRouter::new(
                    &config,
                    Arc::new(Mutex::new(NodeStore::new())),
                    NodeId("source".into()),
                );
                let action = router.wrap_data(
                    &NodeId("destination".into()),
                    chunk.clone(),
                    DeliveryMethod::ReliableOrdered,
                );
                let expected = if unsequenced && bulk_channel {
                    DeliveryMethod::ReliableUnordered
                } else {
                    DeliveryMethod::ReliableOrdered
                };
                assert!(
                    matches!(&action, OverlayAction::SendMessage { method, .. } if *method == expected)
                );
                assert_eq!(unpack(action).0.seq, if unsequenced { 0 } else { 1 });
                for seq in [0, 9] {
                    let envelope = OverlayEnvelope::new(
                        NodeId("origin".into()),
                        NodeId("destination".into()),
                        3,
                        MessageContent::Raw(chunk.clone()),
                    )
                    .with_seq(seq);
                    let action = router
                        .handle_envelope(envelope, NodeId("neighbor".into()))
                        .actions
                        .into_iter()
                        .find(|a| matches!(a, OverlayAction::SendMessage { .. }))
                        .unwrap();
                    let expected = if seq == 0 && bulk_channel {
                        DeliveryMethod::ReliableUnordered
                    } else {
                        DeliveryMethod::ReliableOrdered
                    };
                    assert!(
                        matches!(&action, OverlayAction::SendMessage { method, .. } if *method == expected)
                    );
                    assert_eq!(unpack(action).0.seq, seq);
                }
            }
        }
    }
}
