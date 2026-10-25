#[path = "../../src/transport/webrtc/send_queue.rs"]
mod send_queue;

use bytes::Bytes;
use mistlib_core::types::{ConnectionState, DeliveryMethod};
use send_queue::{send_action, SendAction, SendQueue, MAX_QUEUED_BYTES, MAX_QUEUED_MESSAGES};

#[test]
fn reliable_send_decision_table() {
    use ConnectionState::*;
    use SendAction::*;
    // peer, logical state, actual channel open, queued frames, expected action
    let cases = [
        (true, Reconnecting, true, false, Write),
        (true, Reconnecting, true, true, FlushThenWrite),
        (true, Connecting, true, false, Write),
        (true, Connected, true, true, FlushThenWrite),
        (true, Disconnected, true, false, Write),
        (true, Failed, true, true, FlushThenWrite),
        (true, Reconnecting, false, false, Defer),
        (true, Connecting, false, true, Defer),
        (true, Disconnected, false, true, Fail),
        (true, Failed, false, false, Fail),
        (true, Connected, false, false, Fail),
        (false, Connecting, false, false, Fail),
        (false, Reconnecting, false, true, Fail),
        (false, Disconnected, false, false, Fail),
    ];
    for (peer, state, open, queued, expected) in cases {
        assert_eq!(
            send_action(DeliveryMethod::ReliableOrdered, peer, state, open, queued),
            expected,
            "peer={peer}, state={state:?}, open={open}, queued={queued}"
        );
    }
}

#[test]
fn unreliable_methods_never_defer_or_flush_the_reliable_queue() {
    for method in [
        DeliveryMethod::UnreliableOrdered,
        DeliveryMethod::Unreliable,
    ] {
        for state in [
            ConnectionState::Connected,
            ConnectionState::Connecting,
            ConnectionState::Reconnecting,
            ConnectionState::Disconnected,
            ConnectionState::Failed,
        ] {
            for queued in [false, true] {
                assert_eq!(
                    send_action(method, true, state, true, queued),
                    SendAction::Write
                );
                assert_eq!(
                    send_action(method, true, state, false, queued),
                    SendAction::Fail
                );
                assert_eq!(
                    send_action(method, false, state, false, queued),
                    SendAction::Fail
                );
            }
        }
    }
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
fn queued_frames_stay_in_fifo_until_successfully_written() {
    let mut queue = SendQueue::default();
    assert!(queue.is_empty());
    queue.push(Bytes::from_static(b"a"));
    queue.push(Bytes::from_static(b"b"));
    queue.push(Bytes::from_static(b"c"));

    // Inspecting a head for a write attempt leaves it queued. A failure
    // before pop_front cannot discard it or allow the next frame past it.
    assert_eq!(queue.front(), Some(&Bytes::from_static(b"a")));
    assert_eq!(queue.len(), 3);
    assert_eq!(queue.front(), Some(&Bytes::from_static(b"a")));
    for expected in [b"a", b"b", b"c"] {
        assert_eq!(queue.pop_front(), Some(Bytes::copy_from_slice(expected)));
    }
    assert!(queue.is_empty());
    assert!(queue.pop_front().is_none());
    assert!(queue.front().is_none());
    // Successful pops release the byte budget as well as the message slots.
    assert!(!queue.push(Bytes::from(vec![0u8; MAX_QUEUED_BYTES])));
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
