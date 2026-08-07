#![allow(dead_code)]

#[path = "../src/transport/webrtc/negotiation_delivery.rs"]
mod negotiation_delivery;

use mistlib_core::types::NodeId;
use negotiation_delivery::{
    NegotiationDelivery, TrackStatus, MAX_NEGOTIATIONS_PER_NODE, MAX_NEGOTIATION_NODES,
};

#[test]
fn ack_retires_only_the_matching_transaction() {
    let node = NodeId("node-a".to_string());
    let mut delivery = NegotiationDelivery::default();
    assert_eq!(delivery.track(node.clone(), 10), TrackStatus::New);
    assert_eq!(delivery.track(node.clone(), 20), TrackStatus::New);

    assert!(delivery.acknowledge(&node, 10));
    assert!(!delivery.contains(&node, 10));
    assert!(delivery.contains(&node, 20));
    assert_eq!(delivery.pending_count(&node), 1);
}

#[test]
fn received_transaction_is_deduplicated_only_after_success_is_recorded() {
    let node = NodeId("node-a".to_string());
    let mut delivery = NegotiationDelivery::default();

    assert!(!delivery.is_received(&node, 42));
    assert!(delivery.remember_received(node.clone(), 42));
    assert!(delivery.is_received(&node, 42));
    assert!(!delivery.remember_received(node.clone(), 42));
}

#[test]
fn received_history_is_bounded_and_keeps_the_newest_transactions() {
    let node = NodeId("node-a".to_string());
    let mut delivery = NegotiationDelivery::default();
    for id in 0..MAX_NEGOTIATIONS_PER_NODE as u64 + 1 {
        assert!(delivery.remember_received(node.clone(), id));
    }

    assert!(!delivery.is_received(&node, 0));
    assert!(delivery.is_received(&node, MAX_NEGOTIATIONS_PER_NODE as u64));
}

#[test]
fn node_capacity_and_cleanup_are_bounded() {
    let mut delivery = NegotiationDelivery::default();
    for index in 0..MAX_NEGOTIATION_NODES {
        assert_eq!(
            delivery.track(NodeId(format!("node-{index}")), index as u64),
            TrackStatus::New
        );
    }
    assert_eq!(
        delivery.track(NodeId("overflow".to_string()), 1),
        TrackStatus::AtCapacity
    );

    let node = NodeId("node-0".to_string());
    delivery.remember_received(node.clone(), 77);
    delivery.remove_node(&node);
    assert!(!delivery.contains(&node, 0));
    assert!(!delivery.is_received(&node, 77));
}
