#[path = "../src/transport/webrtc/send_queue.rs"]
mod send_queue;

use bytes::Bytes;
use mistlib_core::types::{ConnectionState, DeliveryMethod};
use send_queue::{should_queue_reliable_send, SendQueue, MAX_QUEUED_BYTES, MAX_QUEUED_MESSAGES};

#[test]
fn reliable_ordered_queues_while_connecting_with_existing_peer() {
    // Fresh connection, or a peer mid ICE-restart grace whose DataChannel
    // isn't open yet (recovery::state_after_ice_recovery keeps it
    // Connecting in that case).
    assert!(should_queue_reliable_send(
        DeliveryMethod::ReliableOrdered,
        true,
        ConnectionState::Connecting
    ));
}

#[test]
fn reliable_ordered_queues_while_reconnecting() {
    assert!(should_queue_reliable_send(
        DeliveryMethod::ReliableOrdered,
        true,
        ConnectionState::Reconnecting
    ));
}

#[test]
fn reliable_ordered_does_not_queue_without_a_peer() {
    assert!(!should_queue_reliable_send(
        DeliveryMethod::ReliableOrdered,
        false,
        ConnectionState::Connecting
    ));
}

#[test]
fn reliable_ordered_does_not_queue_when_disconnected_or_failed() {
    assert!(!should_queue_reliable_send(
        DeliveryMethod::ReliableOrdered,
        true,
        ConnectionState::Disconnected
    ));
    assert!(!should_queue_reliable_send(
        DeliveryMethod::ReliableOrdered,
        true,
        ConnectionState::Failed
    ));
}

#[test]
fn unreliable_methods_never_queue_even_mid_recovery() {
    assert!(!should_queue_reliable_send(
        DeliveryMethod::UnreliableOrdered,
        true,
        ConnectionState::Connecting
    ));
    assert!(!should_queue_reliable_send(
        DeliveryMethod::Unreliable,
        true,
        ConnectionState::Reconnecting
    ));
}

#[test]
fn push_drops_oldest_message_past_the_count_cap() {
    let mut queue = SendQueue::default();
    for i in 0..MAX_QUEUED_MESSAGES {
        assert!(
            !queue.push(Bytes::from_static(b"x")),
            "unexpected drop at message {i}"
        );
    }
    assert_eq!(queue.len(), MAX_QUEUED_MESSAGES);

    assert!(queue.push(Bytes::from_static(b"one-too-many")));
    assert_eq!(queue.len(), MAX_QUEUED_MESSAGES);
}

#[test]
fn push_drops_oldest_message_past_the_byte_cap() {
    let mut queue = SendQueue::default();
    assert!(!queue.push(Bytes::from(vec![0u8; MAX_QUEUED_BYTES])));
    assert_eq!(queue.len(), 1);

    // Any additional byte pushes the running total over budget and evicts
    // the first message to make room.
    assert!(queue.push(Bytes::from_static(b"x")));
    assert_eq!(queue.len(), 1);
}

#[test]
fn drain_returns_fifo_order_and_empties_the_queue() {
    let mut queue = SendQueue::default();
    queue.push(Bytes::from_static(b"a"));
    queue.push(Bytes::from_static(b"b"));
    queue.push(Bytes::from_static(b"c"));

    assert_eq!(
        queue.drain(),
        vec![
            Bytes::from_static(b"a"),
            Bytes::from_static(b"b"),
            Bytes::from_static(b"c"),
        ]
    );
    assert_eq!(queue.len(), 0);

    // Draining an already-empty queue is a no-op, not an error.
    assert!(queue.drain().is_empty());
}

#[test]
fn clear_reports_how_many_were_dropped() {
    let mut queue = SendQueue::default();
    assert_eq!(queue.clear(), 0);

    queue.push(Bytes::from_static(b"a"));
    queue.push(Bytes::from_static(b"b"));
    assert_eq!(queue.clear(), 2);
    assert_eq!(queue.len(), 0);
}
