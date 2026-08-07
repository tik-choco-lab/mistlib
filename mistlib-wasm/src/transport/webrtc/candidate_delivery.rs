use mistlib_core::{signaling::CandidateAck, types::NodeId};
use std::collections::{HashMap, HashSet};

pub const MAX_TRACKED_CANDIDATES_PER_NODE: usize = u64::BITS as usize;
pub const MAX_TRACKED_CANDIDATE_NODES: usize = 256;
pub const CANDIDATE_ACK_DEBOUNCE_MS: u32 = 50;
pub const CANDIDATE_RETRY_DELAYS_MS: [u32; 4] = [300, 700, 1_500, 3_000];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DeliveryKey {
    node: NodeId,
    generation: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackStatus {
    New,
    AlreadyTracked,
    AtCapacity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiveStatus {
    pub is_new: bool,
    pub schedule_ack: bool,
}

#[derive(Default)]
pub struct CandidateDelivery {
    pending: HashMap<DeliveryKey, u64>,
    received: HashMap<DeliveryKey, u64>,
    ack_pending: HashMap<DeliveryKey, u64>,
    ack_scheduled: HashSet<DeliveryKey>,
}

impl CandidateDelivery {
    pub fn track(&mut self, node: NodeId, generation: u32, sequence: u8) -> TrackStatus {
        let Some(bit) = candidate_bit(sequence) else {
            return TrackStatus::AtCapacity;
        };
        let key = DeliveryKey { node, generation };
        if self.pending.get(&key).is_some_and(|mask| mask & bit != 0) {
            return TrackStatus::AlreadyTracked;
        }
        if !self.pending.contains_key(&key) && self.pending.len() >= MAX_TRACKED_CANDIDATE_NODES {
            return TrackStatus::AtCapacity;
        }
        *self.pending.entry(key).or_default() |= bit;
        TrackStatus::New
    }

    pub fn contains(&self, node: &NodeId, generation: u32, sequence: u8) -> bool {
        let Some(bit) = candidate_bit(sequence) else {
            return false;
        };
        let key = DeliveryKey {
            node: node.clone(),
            generation,
        };
        self.pending.get(&key).is_some_and(|mask| mask & bit != 0)
    }

    pub fn acknowledge(&mut self, node: &NodeId, ack: &CandidateAck) -> usize {
        let key = DeliveryKey {
            node: node.clone(),
            generation: ack.generation,
        };
        let Some(mask) = self.pending.get_mut(&key) else {
            return 0;
        };
        let removed = (*mask & ack.mask).count_ones() as usize;
        *mask &= !ack.mask;
        if *mask == 0 {
            self.pending.remove(&key);
        }
        removed
    }

    /// Records a received sequence and queues it for a small batched ACK.
    /// Duplicate retries are ACKed again but are not passed to addIceCandidate.
    pub fn remember_received(
        &mut self,
        node: NodeId,
        generation: u32,
        sequence: u8,
    ) -> ReceiveStatus {
        let Some(bit) = candidate_bit(sequence) else {
            return ReceiveStatus {
                is_new: true,
                schedule_ack: false,
            };
        };
        let key = DeliveryKey { node, generation };
        let is_new = !self.received.get(&key).is_some_and(|mask| mask & bit != 0);
        if is_new {
            if !self.received.contains_key(&key)
                && self.received.len() >= MAX_TRACKED_CANDIDATE_NODES
            {
                return ReceiveStatus {
                    is_new: true,
                    schedule_ack: false,
                };
            }
            *self.received.entry(key.clone()).or_default() |= bit;
        }
        *self.ack_pending.entry(key.clone()).or_default() |= bit;
        let schedule_ack = self.ack_scheduled.insert(key);
        ReceiveStatus {
            is_new,
            schedule_ack,
        }
    }

    pub fn take_ack(&mut self, node: &NodeId, generation: u32) -> Option<CandidateAck> {
        let key = DeliveryKey {
            node: node.clone(),
            generation,
        };
        self.ack_scheduled.remove(&key);
        let mask = self.ack_pending.remove(&key)?;
        Some(CandidateAck { generation, mask })
    }

    pub fn expire(&mut self, node: &NodeId, generation: u32, sequence: u8) -> bool {
        let Some(bit) = candidate_bit(sequence) else {
            return false;
        };
        let key = DeliveryKey {
            node: node.clone(),
            generation,
        };
        let Some(mask) = self.pending.get_mut(&key) else {
            return false;
        };
        let removed = *mask & bit != 0;
        *mask &= !bit;
        if *mask == 0 {
            self.pending.remove(&key);
        }
        removed
    }

    pub fn remove_node(&mut self, node: &NodeId) {
        self.pending.retain(|key, _| &key.node != node);
        self.received.retain(|key, _| &key.node != node);
        self.ack_pending.retain(|key, _| &key.node != node);
        self.ack_scheduled.retain(|key| &key.node != node);
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.received.clear();
        self.ack_pending.clear();
        self.ack_scheduled.clear();
    }

    #[cfg(test)]
    pub fn pending_count(&self, node: &NodeId, generation: u32) -> usize {
        let key = DeliveryKey {
            node: node.clone(),
            generation,
        };
        self.pending
            .get(&key)
            .map_or(0, |mask| mask.count_ones() as usize)
    }
}

fn candidate_bit(sequence: u8) -> Option<u64> {
    1_u64.checked_shl(u32::from(sequence))
}
