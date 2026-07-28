//! Coverage for `Peer::spawn_send_queue` (`transports/webrtc/peer.rs`), the
//! per-peer ordered send queue introduced to fix the self-inflicted
//! reordering bug described in the reorder-reliability notes.
//!
//! IMPORTANT SCOPE NOTE: the actual bug (and its regression test) lives one
//! layer up, at `MistEngine::handle_action_for`/`handle_action_in_room`
//! (`engine/action.rs`; regression test in `engine::tests::action_ordering`).
//! This queue only preserves the order messages are *enqueued* in -- it
//! cannot and does not make N genuinely-concurrently-`tokio::spawn`ed callers
//! of `WebRtcTransport::send`/`try_enqueue_send` arrive in their spawn order,
//! because tokio's scheduler makes no such guarantee between independently
//! spawned tasks (confirmed empirically: an earlier version of this test
//! spawned one task per send, exactly like `handle_action_for` used to, and
//! still observed reordering even with this queue in place). The real fix is
//! that `handle_action_for` no longer spawns a task per `SendMessage` at all
//! -- it calls `try_enqueue_send`/`try_enqueue_broadcast` inline, so the
//! enqueue order matches the caller's true call order. What this queue does
//! guarantee, and what's covered here, is: (1) sequential/awaited sends are
//! written to the DataChannel in that same order, and (2) it's bounded, with
//! overflow dropped (not deadlocked or unbounded) once
//! `PEER_SEND_QUEUE_CAPACITY` is exceeded.
use super::*;
use crate::transports::webrtc::peer::PEER_SEND_QUEUE_CAPACITY;
use crate::transports::webrtc::tests::disconnect::{make_connected_pair, wait_for_state};
use bytes::Bytes;
use mistlib_core::transport::{NetworkEvent, NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct OrderRecorder {
    seen: Mutex<Vec<u32>>,
}

impl NetworkEventHandler for OrderRecorder {
    fn on_event(&self, event: NetworkEvent) {
        if event.data.len() != 4 {
            return;
        }
        let n = u32::from_be_bytes(event.data[..4].try_into().unwrap());
        self.seen.lock().unwrap().push(n);
    }
}

struct NoopEventHandler;
impl NetworkEventHandler for NoopEventHandler {
    fn on_event(&self, _event: NetworkEvent) {}
}

/// Connects `ta` -> `id_b` and returns the pair once both sides report
/// `Connected`. `recorder`/the no-op handler must already be wired via
/// `start()` *before* this is called: `create_pc`/`setup_outgoing_data_channels`
/// snapshot `self.event_handler` at data-channel-creation time, so a handler
/// registered after the connection is already up would never see inbound
/// messages at all (they get silently dropped as "no event handler").
async fn connect_pair_with_recorder() -> (
    Arc<WebRtcTransport>,
    Arc<WebRtcTransport>,
    NodeId,
    NodeId,
    Arc<OrderRecorder>,
) {
    let (ta, tb, id_a, id_b) = make_connected_pair();
    let recorder = Arc::new(OrderRecorder {
        seen: Mutex::new(Vec::new()),
    });
    ta.start(Arc::new(NoopEventHandler)).await.unwrap();
    tb.start(recorder.clone()).await.unwrap();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    (ta, tb, id_a, id_b, recorder)
}

/// Baseline sanity check: sequential, awaited sends to the same peer arrive
/// in the order they were sent -- i.e. the queue itself doesn't reorder or
/// corrupt anything under ordinary, non-concurrent use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_sends_to_same_peer_arrive_in_order() {
    let (ta, _tb, _id_a, id_b, recorder) = connect_pair_with_recorder().await;

    const COUNT: u32 = 200;
    for i in 0..COUNT {
        ta.send(
            &id_b,
            Bytes::copy_from_slice(&i.to_be_bytes()),
            DeliveryMethod::ReliableOrdered,
        )
        .await
        .expect("send should succeed while the channel is open");
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if recorder.seen.lock().unwrap().len() as u32 >= COUNT {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let seen = recorder.seen.lock().unwrap().clone();
    let expected: Vec<u32> = (0..COUNT).collect();
    assert_eq!(seen.len(), COUNT as usize, "not all messages arrived");
    assert_eq!(seen, expected, "sequential sends must arrive in order");
}

/// SPEC-13-style bound check for `PEER_SEND_QUEUE_CAPACITY` (task #3 in
/// the reorder-reliability notes): once more than
/// `PEER_SEND_QUEUE_CAPACITY` messages are enqueued for a peer without the
/// drainer having a chance to catch up, further enqueues must fail fast
/// (`try_send` on a bounded channel) rather than growing unboundedly or
/// blocking the caller.
#[tokio::test]
async fn send_queue_rejects_once_capacity_is_exceeded() {
    let t = make_transport();
    let node = NodeId("overflow-peer".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("create_pc should succeed");

    // A real (but never-opened) DataChannel -- not just an absent one: with
    // no channel at all for `ReliableOrdered`, `Peer::spawn_send_queue`'s
    // drainer treats every dequeued message as immediately undeliverable and
    // drops it right away (see its `None` arm), which would drain the queue
    // as fast as it fills and never actually exercise the capacity bound.
    // Creating a channel that's never connected to a remote peer leaves it
    // stuck in a non-`Open` `ready_state()` forever, so the drainer instead
    // waits on it -- exactly the backed-up state needed to prove the bound.
    let dc = peer
        .pc
        .create_data_channel("reliable", None)
        .await
        .expect("create_data_channel should succeed even with no remote peer");
    peer.channels
        .write()
        .await
        .insert(DeliveryMethod::ReliableOrdered, dc);
    t.peers.write().await.insert(node.clone(), peer);

    // Send well over capacity. Exactly how many of the first sends succeed
    // before the first rejection depends on whether the drainer task has
    // already dequeued its first (currently-blocked-on-`Open`) message by
    // the time this loop reaches the channel's actual capacity boundary --
    // that's a one-item race against the drainer, not something worth
    // pinning down exactly, so this only asserts the bound is enforced at
    // all: both that overflow eventually happens, and that it doesn't take
    // wildly more than `PEER_SEND_QUEUE_CAPACITY` sends to trigger it.
    let attempts = PEER_SEND_QUEUE_CAPACITY * 4;
    let mut accepted = 0usize;
    let mut first_rejection: Option<usize> = None;
    for i in 0..attempts {
        let result = t
            .send(
                &node,
                Bytes::copy_from_slice(&(i as u32).to_be_bytes()),
                DeliveryMethod::ReliableOrdered,
            )
            .await;
        if result.is_ok() {
            accepted += 1;
        } else if first_rejection.is_none() {
            first_rejection = Some(i);
        }
    }

    assert!(
        first_rejection.is_some(),
        "sending {attempts} messages to a peer whose channel never opens must eventually be \
         rejected once the queue is full, not accepted unboundedly"
    );
    assert!(
        accepted <= PEER_SEND_QUEUE_CAPACITY + 1,
        "at most PEER_SEND_QUEUE_CAPACITY (+1 for the drainer's own in-flight item) sends should \
         ever be accepted while the channel never opens, got {accepted}"
    );
}
