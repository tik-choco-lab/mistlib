use mistlib_core::types::NodeId;
use std::collections::{HashMap, HashSet, VecDeque};

pub const MAX_NEGOTIATION_NODES: usize = 256;
pub const MAX_NEGOTIATIONS_PER_NODE: usize = 64;
pub const NEGOTIATION_RETRY_DELAYS_MS: [u32; 4] = [300, 700, 1_500, 3_000];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackStatus {
    New,
    AlreadyTracked,
    AtCapacity,
}

#[derive(Default)]
pub struct NegotiationDelivery {
    pending: HashMap<NodeId, HashSet<u64>>,
    received: HashMap<NodeId, VecDeque<u64>>,
}

impl NegotiationDelivery {
    pub fn track(&mut self, node: NodeId, id: u64) -> TrackStatus {
        if self
            .pending
            .get(&node)
            .is_some_and(|items| items.contains(&id))
        {
            return TrackStatus::AlreadyTracked;
        }
        if !self.pending.contains_key(&node) && self.pending.len() >= MAX_NEGOTIATION_NODES {
            return TrackStatus::AtCapacity;
        }
        let items = self.pending.entry(node).or_default();
        if items.len() >= MAX_NEGOTIATIONS_PER_NODE {
            return TrackStatus::AtCapacity;
        }
        items.insert(id);
        TrackStatus::New
    }

    pub fn contains(&self, node: &NodeId, id: u64) -> bool {
        self.pending
            .get(node)
            .is_some_and(|items| items.contains(&id))
    }

    pub fn acknowledge(&mut self, node: &NodeId, id: u64) -> bool {
        let Some(items) = self.pending.get_mut(node) else {
            return false;
        };
        let removed = items.remove(&id);
        if items.is_empty() {
            self.pending.remove(node);
        }
        removed
    }

    pub fn expire(&mut self, node: &NodeId, id: u64) -> bool {
        self.acknowledge(node, id)
    }

    pub fn is_received(&self, node: &NodeId, id: u64) -> bool {
        self.received
            .get(node)
            .is_some_and(|items| items.contains(&id))
    }

    pub fn remember_received(&mut self, node: NodeId, id: u64) -> bool {
        if self.is_received(&node, id) {
            return false;
        }
        if !self.received.contains_key(&node) && self.received.len() >= MAX_NEGOTIATION_NODES {
            return false;
        }
        let items = self.received.entry(node).or_default();
        if items.len() >= MAX_NEGOTIATIONS_PER_NODE {
            items.pop_front();
        }
        items.push_back(id);
        true
    }

    pub fn remove_node(&mut self, node: &NodeId) {
        self.pending.remove(node);
        self.received.remove(node);
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.received.clear();
    }

    #[cfg(test)]
    pub fn pending_count(&self, node: &NodeId) -> usize {
        self.pending.get(node).map_or(0, HashSet::len)
    }
}
