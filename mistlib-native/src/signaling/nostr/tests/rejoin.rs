//! Regression tests for the Nostr signaling rejoin fix: a node's signaling
//! keypair regenerates on every restart while its `NodeId` stays stable, so
//! after a restart the same `NodeId` arrives under a new pubkey. These cover
//! the epoch-aware rebind (`DiscoveryTable::bind_node_with_epoch`) and the
//! locally-synthesized `SignalingType::Rejoin` notification the transport
//! uses to tear down the stale peer connection -- see `processing.rs` and
//! `SignalingType::Rejoin`'s doc comment.

use super::super::NostrSignaler;
use super::{config, data, recv_available};
use mistlib_core::signaling::nostr::build_message_event_with_sequence_and_joined_at;
use mistlib_core::signaling::{MessageContent, SignalingType};
use mistlib_core::types::NodeId;
use tokio::sync::mpsc;

const OLD_EPOCH: u64 = 1_000;

/// Binds `bob_id` in `alice`'s discovery table to `old_bob`'s identity at
/// [`OLD_EPOCH`] via an ordinary `Request` message (the only message type
/// that can establish a first-time binding without needing
/// `requested_pubkeys` pre-populated), draining the message it delivers.
async fn seed_initial_binding(
    alice: &NostrSignaler,
    old_bob: &NostrSignaler,
    bob_id: &NodeId,
    alice_id: &NodeId,
    room_id: &str,
    tx: &mpsc::Sender<MessageContent>,
    rx: &mut mpsc::Receiver<MessageContent>,
) {
    let old_bob_identity = old_bob.current_identity().await;
    let alice_identity = alice.current_identity().await;
    let first = build_message_event_with_sequence_and_joined_at(
        &old_bob.codec_config,
        &old_bob.crypto,
        &old_bob_identity,
        &alice_identity.public_key,
        &data(bob_id, alice_id, room_id, SignalingType::Request),
        1,
        Some(OLD_EPOCH),
    )
    .unwrap();
    alice.process_event(first, tx.clone()).await.unwrap();
    assert_eq!(
        alice.discovery_table.lock().await.pubkey_for_node(bob_id),
        Some(old_bob_identity.public_key.clone()),
        "sanity: initial binding to the old pubkey must succeed"
    );
    // Drain the Request delivery itself; every test below only cares about
    // what happens on the *next* message.
    let _ = rx.recv().await;
}

/// Scenario 1: a peer known as `(node B, pubkey P1, epoch E1)` sends a
/// non-`Request` message under a brand-new, never-requested pubkey P2 with
/// `sender_joined_at = E1 + 1`. This must be accepted purely on the epoch
/// comparison -- `allow_rebind` (`sender_was_requested && Request`) is false
/// here since the message is a `Candidate` and P2 was never requested,
/// isolating the new epoch-based path from the pre-existing escape hatch.
/// Deliberately does NOT pre-seed `requested_pubkeys` for P2: a peer that
/// restarts and is the deterministic WebRTC offerer for this pair sends a
/// non-`Request` message (an `Offer`) as its very first contact, under a
/// pubkey the receiver could not possibly have requested yet -- see
/// `accept_sender_for_payload`'s doc comment for why the core
/// already-requested-or-Request gate alone would drop this before the rebind
/// logic below is ever reached. The table must rebind to P2, and a `Rejoin`
/// notification for B must be emitted on the incoming stream *before* the
/// triggering message.
#[tokio::test]
async fn rejoin_with_newer_epoch_rebinds_and_emits_rejoin_before_the_message() {
    let room_id = "room-rejoin-epoch";
    let alice_id = NodeId("alice".to_string());
    let bob_id = NodeId("bob".to_string());
    let alice = NostrSignaler::new(alice_id.clone(), config());
    let old_bob = NostrSignaler::new(bob_id.clone(), config());
    let new_bob = NostrSignaler::new(bob_id.clone(), config());
    alice.set_room_id(room_id).await.unwrap();
    let (tx, mut rx) = mpsc::channel(8);

    seed_initial_binding(&alice, &old_bob, &bob_id, &alice_id, room_id, &tx, &mut rx).await;
    let old_bob_pubkey = old_bob.current_identity().await.public_key;
    let new_bob_identity = new_bob.current_identity().await;

    let new_epoch = OLD_EPOCH + 1;
    let alice_identity = alice.current_identity().await;
    let rejoin_trigger = build_message_event_with_sequence_and_joined_at(
        &new_bob.codec_config,
        &new_bob.crypto,
        &new_bob_identity,
        &alice_identity.public_key,
        &data(&bob_id, &alice_id, room_id, SignalingType::Candidate),
        1,
        Some(new_epoch),
    )
    .unwrap();
    alice
        .process_event(rejoin_trigger, tx.clone())
        .await
        .expect("a strictly newer epoch must be accepted");

    assert_eq!(
        alice.discovery_table.lock().await.pubkey_for_node(&bob_id),
        Some(new_bob_identity.public_key.clone()),
        "the table must rebind node B to the new pubkey"
    );

    let first = rx
        .recv()
        .await
        .expect("a Rejoin notification must be emitted");
    let MessageContent::Data(rejoin) = first else {
        panic!("unexpected message content: {first:?}");
    };
    assert_eq!(rejoin.signaling_type, SignalingType::Rejoin);
    assert_eq!(rejoin.sender_id, bob_id);
    assert_eq!(rejoin.receiver_id, alice_id);
    assert_eq!(
        rejoin.data,
        new_epoch.to_string(),
        "the Rejoin payload must carry the peer's new joined_at as a decimal string"
    );

    let second = rx
        .recv()
        .await
        .expect("the triggering message must follow the Rejoin notification");
    let MessageContent::Data(triggering) = second else {
        panic!("unexpected message content: {second:?}");
    };
    assert_eq!(triggering.signaling_type, SignalingType::Candidate);
    assert_eq!(triggering.sender_id, bob_id);

    assert!(
        recv_available(&mut rx).await.is_none(),
        "exactly two messages (Rejoin then the trigger) must be emitted, nothing more"
    );

    // Sanity: the old pubkey is no longer bound to anything discoverable.
    assert!(alice
        .discovery_table
        .lock()
        .await
        .expires_at_for_pubkey(&old_bob_pubkey)
        .is_none());
}

/// Regression test for the adversarial-review finding: native's
/// `accept_sender_for_payload` gate used to be a bare passthrough to the core
/// check (already-known-by-this-pubkey, or previously `Request`-ed), which
/// ran *before* the epoch-aware rebind and dropped everything else -- making
/// the whole rejoin fix inert for exactly the message that matters most in
/// practice. The WebRTC-level "who offers first" decision
/// (`local_node_id < remote.node_id`, in `transports/webrtc.rs`) is a raw
/// `NodeId` comparison with no relationship to the Nostr discovery-rank
/// protocol that decides who sends a `Request` -- so a restarted peer that
/// happens to be the deterministic offerer for a given pair sends an `Offer`,
/// never a `Request`, as its first message under its fresh pubkey, and that
/// pubkey has by construction never been requested by the receiver. This
/// reproduces exactly that: node B rejoins and immediately sends an `Offer`
/// (not a `Request`) under a never-requested pubkey with a strictly-newer
/// epoch -- it must be accepted, the table must rebind, and a `Rejoin` must
/// be emitted before the `Offer` is forwarded.
#[tokio::test]
async fn rejoin_via_first_post_restart_offer_from_never_requested_pubkey_is_accepted() {
    let room_id = "room-rejoin-first-offer";
    let alice_id = NodeId("alice".to_string());
    let bob_id = NodeId("bob".to_string());
    let alice = NostrSignaler::new(alice_id.clone(), config());
    let old_bob = NostrSignaler::new(bob_id.clone(), config());
    let new_bob = NostrSignaler::new(bob_id.clone(), config());
    alice.set_room_id(room_id).await.unwrap();
    let (tx, mut rx) = mpsc::channel(8);

    seed_initial_binding(&alice, &old_bob, &bob_id, &alice_id, room_id, &tx, &mut rx).await;

    // `new_bob`'s pubkey is never inserted into `alice.requested_pubkeys`
    // anywhere in this test -- the whole point is that the receiver had no
    // opportunity to request it before this Offer arrives.
    let new_bob_identity = new_bob.current_identity().await;
    let new_epoch = OLD_EPOCH + 1;
    let alice_identity = alice.current_identity().await;
    let offer = build_message_event_with_sequence_and_joined_at(
        &new_bob.codec_config,
        &new_bob.crypto,
        &new_bob_identity,
        &alice_identity.public_key,
        &data(&bob_id, &alice_id, room_id, SignalingType::Offer),
        1,
        Some(new_epoch),
    )
    .unwrap();

    alice
        .process_event(offer, tx.clone())
        .await
        .expect("a never-requested pubkey with a strictly newer epoch must be accepted");

    assert_eq!(
        alice.discovery_table.lock().await.pubkey_for_node(&bob_id),
        Some(new_bob_identity.public_key.clone()),
        "the table must rebind node B to the new pubkey from the Offer alone"
    );

    let first = rx
        .recv()
        .await
        .expect("a Rejoin notification must be emitted");
    let MessageContent::Data(rejoin) = first else {
        panic!("unexpected message content: {first:?}");
    };
    assert_eq!(rejoin.signaling_type, SignalingType::Rejoin);
    assert_eq!(rejoin.sender_id, bob_id);

    let second = rx
        .recv()
        .await
        .expect("the Offer must follow the Rejoin notification");
    let MessageContent::Data(triggering) = second else {
        panic!("unexpected message content: {second:?}");
    };
    assert_eq!(triggering.signaling_type, SignalingType::Offer);
    assert_eq!(triggering.sender_id, bob_id);
}

/// Companion to the test above: an unrequested sender presenting an equal or
/// older epoch must still be dropped by the new admission fallback in
/// `accept_sender_for_payload` itself -- silently (`Ok(())`, nothing
/// forwarded), since this case never even reaches `bind_node_with_epoch`.
/// This is the "impostor claims a node id with a stale epoch, and was never
/// requested either" case the epoch fallback must not admit.
#[tokio::test]
async fn unrequested_sender_with_stale_epoch_is_dropped_by_the_admission_gate() {
    let room_id = "room-rejoin-first-offer-guard";
    let alice_id = NodeId("alice".to_string());
    let bob_id = NodeId("bob".to_string());
    let alice = NostrSignaler::new(alice_id.clone(), config());
    let old_bob = NostrSignaler::new(bob_id.clone(), config());
    alice.set_room_id(room_id).await.unwrap();
    let (tx, mut rx) = mpsc::channel(8);

    seed_initial_binding(&alice, &old_bob, &bob_id, &alice_id, room_id, &tx, &mut rx).await;

    for (label, candidate_epoch) in [("equal", OLD_EPOCH), ("older", OLD_EPOCH - 1), ("none", 0)] {
        let impostor_bob = NostrSignaler::new(bob_id.clone(), config());
        let impostor_identity = impostor_bob.current_identity().await;
        let alice_identity = alice.current_identity().await;
        let sender_epoch = (label != "none").then_some(candidate_epoch);
        let attempt = build_message_event_with_sequence_and_joined_at(
            &impostor_bob.codec_config,
            &impostor_bob.crypto,
            &impostor_identity,
            &alice_identity.public_key,
            &data(&bob_id, &alice_id, room_id, SignalingType::Offer),
            1,
            sender_epoch,
        )
        .unwrap();

        alice
            .process_event(attempt, tx.clone())
            .await
            .expect("dropping at the admission gate must not surface as an error");
        assert_eq!(
            alice.discovery_table.lock().await.pubkey_for_node(&bob_id),
            Some(old_bob.current_identity().await.public_key),
            "the table must still point at the original pubkey after a {label}-epoch, \
             never-requested attempt"
        );
        assert!(
            recv_available(&mut rx).await.is_none(),
            "no Rejoin (or anything else) may be emitted for a dropped {label}-epoch, \
             never-requested attempt"
        );
    }
}

/// Scenario 2: an equal or older `sender_joined_at` must still be rejected
/// (the impersonation guard stays intact) -- a replayed/duplicate epoch, or a
/// hostile peer trying to steal a node id's binding with a stale epoch, must
/// not win a rebind. No `Rejoin` may be emitted for a rejected attempt.
#[tokio::test]
async fn rejoin_with_equal_or_older_epoch_is_rejected_and_emits_no_rejoin() {
    let room_id = "room-rejoin-epoch-guard";
    let alice_id = NodeId("alice".to_string());
    let bob_id = NodeId("bob".to_string());
    let alice = NostrSignaler::new(alice_id.clone(), config());
    let old_bob = NostrSignaler::new(bob_id.clone(), config());
    alice.set_room_id(room_id).await.unwrap();
    let (tx, mut rx) = mpsc::channel(8);

    seed_initial_binding(&alice, &old_bob, &bob_id, &alice_id, room_id, &tx, &mut rx).await;

    for (label, candidate_epoch) in [("equal", OLD_EPOCH), ("older", OLD_EPOCH - 1)] {
        let impostor_bob = NostrSignaler::new(bob_id.clone(), config());
        let impostor_identity = impostor_bob.current_identity().await;
        alice
            .requested_pubkeys
            .lock()
            .await
            .insert(impostor_identity.public_key.clone());
        let alice_identity = alice.current_identity().await;
        let attempt = build_message_event_with_sequence_and_joined_at(
            &impostor_bob.codec_config,
            &impostor_bob.crypto,
            &impostor_identity,
            &alice_identity.public_key,
            &data(&bob_id, &alice_id, room_id, SignalingType::Candidate),
            1,
            Some(candidate_epoch),
        )
        .unwrap();

        assert!(
            alice.process_event(attempt, tx.clone()).await.is_err(),
            "a {label} epoch must not win a rebind"
        );
        assert_eq!(
            alice.discovery_table.lock().await.pubkey_for_node(&bob_id),
            Some(old_bob.current_identity().await.public_key),
            "the table must still point at the original pubkey after a {label}-epoch attempt"
        );
        assert!(
            recv_available(&mut rx).await.is_none(),
            "no Rejoin (or anything else) may be emitted for a rejected {label}-epoch attempt"
        );
    }
}

/// Scenario 3: every per-peer map keyed by the pubkey a node rebound *away*
/// from must be purged on a detected rejoin, so no stale sequence counters,
/// request bookkeeping, or session state can bleed from the dead identity
/// into the peer's fresh session.
#[tokio::test]
async fn rejoin_purges_per_peer_state_keyed_by_the_old_pubkey() {
    let room_id = "room-rejoin-purge";
    let alice_id = NodeId("alice".to_string());
    let bob_id = NodeId("bob".to_string());
    let alice = NostrSignaler::new(alice_id.clone(), config());
    let old_bob = NostrSignaler::new(bob_id.clone(), config());
    let new_bob = NostrSignaler::new(bob_id.clone(), config());
    alice.set_room_id(room_id).await.unwrap();
    let (tx, mut rx) = mpsc::channel(8);

    seed_initial_binding(&alice, &old_bob, &bob_id, &alice_id, room_id, &tx, &mut rx).await;
    let old_bob_pubkey = old_bob.current_identity().await.public_key;

    // Populate every per-peer map this fix is responsible for purging, keyed
    // by the soon-to-be-dead old pubkey.
    alice
        .requested_pubkeys
        .lock()
        .await
        .insert(old_bob_pubkey.clone());
    alice
        .incoming_sequences
        .lock()
        .await
        .insert(old_bob_pubkey.clone(), 7);
    alice
        .outgoing_sequences
        .lock()
        .await
        .insert(old_bob_pubkey.clone(), 3);
    alice
        .peer_sessions
        .lock()
        .await
        .insert(old_bob_pubkey.clone(), OLD_EPOCH);

    let new_bob_identity = new_bob.current_identity().await;
    alice
        .requested_pubkeys
        .lock()
        .await
        .insert(new_bob_identity.public_key.clone());
    let alice_identity = alice.current_identity().await;
    let rejoin_trigger = build_message_event_with_sequence_and_joined_at(
        &new_bob.codec_config,
        &new_bob.crypto,
        &new_bob_identity,
        &alice_identity.public_key,
        &data(&bob_id, &alice_id, room_id, SignalingType::Candidate),
        1,
        Some(OLD_EPOCH + 1),
    )
    .unwrap();
    alice
        .process_event(rejoin_trigger, tx.clone())
        .await
        .expect("a strictly newer epoch must be accepted");

    assert!(
        !alice
            .requested_pubkeys
            .lock()
            .await
            .contains(&old_bob_pubkey),
        "requested_pubkeys must be purged of the old pubkey"
    );
    assert!(
        !alice
            .incoming_sequences
            .lock()
            .await
            .contains_key(&old_bob_pubkey),
        "incoming_sequences must be purged of the old pubkey"
    );
    assert!(
        !alice
            .outgoing_sequences
            .lock()
            .await
            .contains_key(&old_bob_pubkey),
        "outgoing_sequences must be purged of the old pubkey"
    );
    assert!(
        !alice
            .peer_sessions
            .lock()
            .await
            .contains_key(&old_bob_pubkey),
        "peer_sessions must be purged of the old pubkey"
    );
}

/// Scenario 4a: `Rejoin` must never be accepted from the wire -- a remote
/// peer must never be able to make us tear down a live connection just by
/// publishing a crafted message. The event must be dropped (not forwarded,
/// not erroring the caller) even though it is otherwise validly signed and
/// encrypted.
#[tokio::test]
async fn wire_delivered_rejoin_is_dropped_not_forwarded() {
    let room_id = "room-rejoin-wire-guard";
    let alice_id = NodeId("alice".to_string());
    let mallory_id = NodeId("mallory".to_string());
    let alice = NostrSignaler::new(alice_id.clone(), config());
    let mallory = NostrSignaler::new(mallory_id.clone(), config());
    alice.set_room_id(room_id).await.unwrap();
    let (tx, mut rx) = mpsc::channel(8);

    let mallory_identity = mallory.current_identity().await;
    let alice_identity = alice.current_identity().await;
    let crafted = build_message_event_with_sequence_and_joined_at(
        &mallory.codec_config,
        &mallory.crypto,
        &mallory_identity,
        &alice_identity.public_key,
        &data(&mallory_id, &alice_id, room_id, SignalingType::Rejoin),
        1,
        Some(999_999),
    )
    .unwrap();

    alice
        .process_event(crafted, tx)
        .await
        .expect("a wire-delivered Rejoin must be dropped, not error the caller");

    assert!(
        recv_available(&mut rx).await.is_none(),
        "a wire-delivered Rejoin must never be forwarded to the local incoming stream"
    );
}

/// Scenario 4b: a `Rejoin` must never be published, at either publish entry
/// point (`Signaler::send_signaling` and the lower-level
/// `publish_message_to_pubkey`) -- even when nothing local is expected to
/// ever construct one, both guard unconditionally.
#[tokio::test]
async fn rejoin_is_never_published() {
    use mistlib_core::signaling::Signaler;

    let room_id = "room-rejoin-never-published";
    let alice_id = NodeId("alice".to_string());
    let bob_id = NodeId("bob".to_string());
    let alice = NostrSignaler::new(alice_id.clone(), config());
    let bob = NostrSignaler::new(bob_id.clone(), config());
    let (relay_tx, mut relay_rx) = mpsc::channel(8);
    alice.senders.lock().await.push(relay_tx);
    alice.set_room_id(room_id).await.unwrap();
    while relay_rx.try_recv().is_ok() {}

    let bob_pubkey = bob.current_identity().await.public_key;
    let rejoin_payload = data(&bob_id, &alice_id, room_id, SignalingType::Rejoin);

    alice
        .publish_message_to_pubkey(&bob_pubkey, &rejoin_payload)
        .await
        .expect("publish_message_to_pubkey must treat a Rejoin as a no-op success");
    assert!(
        recv_available_relay(&mut relay_rx).await.is_none(),
        "publish_message_to_pubkey must never put a Rejoin frame on the relay channel"
    );

    alice
        .send_signaling(&bob_id, MessageContent::Data(rejoin_payload))
        .await
        .expect("send_signaling must treat a Rejoin as a no-op success");
    assert!(
        recv_available_relay(&mut relay_rx).await.is_none(),
        "send_signaling must never put a Rejoin frame on the relay channel"
    );

    // Sanity: an ordinary message from the same signaler still publishes
    // normally, proving the guard is specific to `Rejoin` rather than a
    // broken publish path.
    let ordinary = data(&bob_id, &alice_id, room_id, SignalingType::Candidate);
    alice
        .publish_message_to_pubkey(&bob_pubkey, &ordinary)
        .await
        .expect("an ordinary message must still publish");
    assert!(recv_available_relay(&mut relay_rx).await.is_some());
}

async fn recv_available_relay(rx: &mut mpsc::Receiver<String>) -> Option<String> {
    tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
        .await
        .unwrap_or(None)
}
