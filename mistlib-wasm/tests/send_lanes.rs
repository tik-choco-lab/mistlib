// Host tests exercise the exact shared policy used by wasm's core worker.
#[path = "../src/transport/webrtc/lane_policy.rs"]
mod lane_policy;

use mistlib_core::action::SendLane;
use mistlib_core::transport::send::{LaneQueue, ScheduledSend, SendClass};
struct Frame(SendLane, bool);
impl ScheduledSend for Frame {
    fn send_class(&self) -> SendClass {
        SendClass {
            lane: self.0,
            unsequenced_bulk: self.1,
        }
    }
}

#[test]
fn browser_worker_can_dispatch_control_while_bulk_writer_is_blocked() {
    let mut queue = LaneQueue::new(true);
    queue.push_back(Frame(SendLane::Bulk, true));
    queue.push_back(Frame(SendLane::Control, false));
    assert_eq!(queue.pop(true, false).unwrap().0 .0, SendLane::Control);
    assert!(queue.pop(false, false).is_none());
    assert!(queue.pop(false, true).unwrap().1);
}

#[test]
fn browser_worker_keeps_old_positive_bulk_on_the_ordered_writer() {
    let mut queue = LaneQueue::new(true);
    queue.push_back(Frame(SendLane::Bulk, false));
    queue.push_back(Frame(SendLane::Control, false));
    assert!(!queue.pop(true, true).unwrap().1);
    assert!(queue.pop(false, true).is_none());
}

#[test]
fn split_writer_requires_valid_zero_sequence_bulk_and_priority_flag() {
    use mistlib_core::{overlay::OverlayEnvelope, signaling::MessageContent, types::NodeId};
    let cid = mistlib_core::storage::compute_cid(b"data", 0x55);
    let payload = mistlib_core::storage::protocol::build_have_chunk_message(&cid, 0, 1, b"data");
    let envelope = OverlayEnvelope::new(
        NodeId("a".into()),
        NodeId("b".into()),
        1,
        MessageContent::Raw(payload.into()),
    );
    let bytes = mistlib_core::overlay::wire::serialize(&envelope).unwrap();
    assert!(!lane_policy::use_bulk_writer(false, &bytes));
    assert!(lane_policy::use_bulk_writer(true, &bytes));
    let positive = mistlib_core::overlay::wire::serialize(&envelope.with_seq(1)).unwrap();
    assert!(!lane_policy::use_bulk_writer(true, &positive));
    assert!(!lane_policy::use_bulk_writer(true, &[5]));
}

#[path = "../../mistlib-core/src/transport/reliable_unordered.rs"]
mod reliable_unordered;

#[test]
fn explicit_labels_and_pc_generation_capabilities_prevent_reliable_aliasing() {
    assert_eq!(
        reliable_unordered::channel_label("reliable-unordered"),
        Some(reliable_unordered::ChannelLabel::ReliableUnordered)
    );
    assert_eq!(reliable_unordered::channel_label("surprise"), None);
    let old_pc = reliable_unordered::ReliableUnorderedCapability::default();
    old_pc.set_local(true);
    old_pc.set_initial_offerer();
    old_pc.accept_remote(Some(1));
    assert!(old_pc.begin_open());
    old_pc.disable();
    assert!(!old_pc.accepts_channel());
    assert!(!reliable_unordered::ReliableUnorderedCapability::default().accepts_channel());
}

#[test]
fn unreliable_send_wait_leaves_reliable_fifo_untouched() {
    use mistlib_core::types::DeliveryMethod::*;
    for method in [
        Unreliable,
        UnreliableOrdered,
        ReliableOrdered,
        ReliableUnordered,
    ] {
        for bulk in [false, true] {
            let mut reliable_fifo = std::collections::VecDeque::from([b"seq-1", b"seq-2"]);
            let selected = method;
            if lane_policy::flush_reliable_queue(method, bulk) {
                assert_eq!(selected, ReliableOrdered);
                reliable_fifo.clear();
            }
            assert_eq!(
                reliable_fifo.len(),
                if method == ReliableOrdered && !bulk {
                    0
                } else {
                    2
                }
            );
        }
    }
}

#[test]
fn closed_optional_channel_retries_same_frame_but_submitted_failure_does_not() {
    use mistlib_core::transport::send::{closed_send, with_reliable_fallback, WriteFailure};
    futures::executor::block_on(async {
        let frame = b"seq-zero-chunk";
        let writes = std::cell::RefCell::new(Vec::new());
        with_reliable_fallback(
            async { Err(WriteFailure::Uncommitted(closed_send())) },
            || async {
                writes.borrow_mut().push(*frame);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(*writes.borrow(), vec![*frame]);
        assert!(with_reliable_fallback(
            async { Err(WriteFailure::Failed(closed_send())) },
            || async { panic!("ambiguous submission must never retry") }
        )
        .await
        .is_err());
        assert!(with_reliable_fallback(
            async { Err(WriteFailure::Uncommitted(closed_send())) },
            || async { Err(closed_send()) }
        )
        .await
        .is_err());
    });
}

#[test]
fn adaptive_bulk_budget_accounts_shared_control_and_preserves_control_watermark() {
    use mistlib_core::transport::bulk_pacing::BulkLink;
    use mistlib_core::types::DeliveryMethod;
    let mut link = BulkLink::new(100, false);
    link.observe(0, 0, 0, true);
    assert!(link.can_submit(0, 17000));
    link.submitted(0, 17000, true);
    link.submitted(0, 17000, false); // control shares fallback's counter
    link.observe(0, 50, 34000, true); // nothing actually drained
    assert_eq!(link.policy.rate_bytes_per_second(), Some(0.0));
    assert!(!link.can_submit(0, 17000));
    assert_eq!(
        backpressure::backpressure_action(34000, 1024 * 1024, DeliveryMethod::ReliableOrdered),
        backpressure::BackpressureAction::SendNow
    );
    assert_eq!(
        backpressure::drain_timeout_action(34000, 17000, 34000, 8 * 1024 * 1024),
        backpressure::DrainTimeoutAction::SendNow
    );
    link.observe(1, 100, 0, true); // optional bulk channel opened
    assert_eq!(link.policy.rate_bytes_per_second(), None);
    assert_eq!(link.policy.budget(), 65536);
    assert!(link.can_submit(1, 17000));
}

#[path = "../src/transport/webrtc/backpressure.rs"]
mod backpressure;

#[test]
fn reliable_unordered_selects_only_fully_reliable_channels() {
    use mistlib_core::types::DeliveryMethod::*;
    assert_eq!(
        lane_policy::physical_method(ReliableUnordered, true),
        ReliableUnordered
    );
    for unavailable in [false] {
        // absent, connecting, closed all pass false
        assert_eq!(
            lane_policy::physical_method(ReliableUnordered, unavailable),
            ReliableOrdered
        );
    }
    assert_eq!(
        backpressure::backpressure_action(100, 10, ReliableUnordered),
        backpressure::BackpressureAction::WaitThenSend
    );
}
