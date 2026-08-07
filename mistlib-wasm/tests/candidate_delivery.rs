#![allow(dead_code)]
#[path = "../src/transport/webrtc/candidate_delivery.rs"]
mod candidate_delivery;

use candidate_delivery::{CandidateDelivery, TrackStatus, MAX_TRACKED_CANDIDATE_NODES};
use mistlib_core::signaling::CandidateAck;
use mistlib_core::types::NodeId;

#[test]
fn bitmap_ack_retires_only_received_sequences() {
    let node = NodeId("node-a".to_string());
    let mut delivery = CandidateDelivery::default();

    assert_eq!(delivery.track(node.clone(), 7, 0), TrackStatus::New);
    assert_eq!(delivery.track(node.clone(), 7, 2), TrackStatus::New);
    assert_eq!(
        delivery.acknowledge(
            &node,
            &CandidateAck {
                generation: 7,
                mask: 0b0001,
            },
        ),
        1
    );
    assert!(!delivery.contains(&node, 7, 0));
    assert!(delivery.contains(&node, 7, 2));
    assert_eq!(delivery.pending_count(&node, 7), 1);
}

#[test]
fn received_candidates_are_deduplicated_and_acked_as_one_bitmap() {
    let node = NodeId("node-a".to_string());
    let mut delivery = CandidateDelivery::default();

    let first = delivery.remember_received(node.clone(), 11, 1);
    let second = delivery.remember_received(node.clone(), 11, 3);
    let duplicate = delivery.remember_received(node.clone(), 11, 1);

    assert!(first.is_new && first.schedule_ack);
    assert!(second.is_new && !second.schedule_ack);
    assert!(!duplicate.is_new && !duplicate.schedule_ack);
    assert_eq!(
        delivery.take_ack(&node, 11),
        Some(CandidateAck {
            generation: 11,
            mask: 0b1010,
        })
    );
}

#[test]
fn late_ack_from_old_generation_cannot_retire_new_candidate() {
    let node = NodeId("node-a".to_string());
    let mut delivery = CandidateDelivery::default();
    delivery.track(node.clone(), 1, 0);
    delivery.track(node.clone(), 2, 0);

    delivery.acknowledge(
        &node,
        &CandidateAck {
            generation: 1,
            mask: 1,
        },
    );

    assert!(!delivery.contains(&node, 1, 0));
    assert!(delivery.contains(&node, 2, 0));
}

#[test]
fn tracking_is_bounded_and_cleanup_removes_all_generations() {
    let mut delivery = CandidateDelivery::default();
    for index in 0..MAX_TRACKED_CANDIDATE_NODES {
        assert_eq!(
            delivery.track(NodeId(format!("node-{index}")), 1, 0),
            TrackStatus::New
        );
    }
    assert_eq!(
        delivery.track(NodeId("overflow".to_string()), 1, 0),
        TrackStatus::AtCapacity
    );

    let node = NodeId("node-0".to_string());
    delivery.track(node.clone(), 2, 1);
    delivery.remember_received(node.clone(), 3, 2);
    delivery.remove_node(&node);
    assert!(!delivery.contains(&node, 1, 0));
    assert!(!delivery.contains(&node, 2, 1));
    assert_eq!(delivery.take_ack(&node, 3), None);
}
