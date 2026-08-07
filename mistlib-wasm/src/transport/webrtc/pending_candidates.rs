use mistlib_core::types::{ConnectionState, NodeId};
use std::collections::HashMap;

pub const MAX_PENDING_CANDIDATES_PER_NODE: usize = 64;
pub const MAX_PENDING_CANDIDATE_NODES: usize = 256;

#[derive(Default)]
pub struct PendingCandidates {
    inner: HashMap<NodeId, Vec<String>>,
}

impl PendingCandidates {
    pub(crate) fn push(&mut self, node: NodeId, candidate: String) -> bool {
        let list = self.inner.entry(node).or_default();
        list.push(candidate);
        if list.len() > MAX_PENDING_CANDIDATES_PER_NODE {
            list.remove(0);
            true
        } else {
            false
        }
    }

    pub(crate) fn contains_node(&self, node: &NodeId) -> bool {
        self.inner.contains_key(node)
    }

    pub(crate) fn node_count(&self) -> usize {
        self.inner.len()
    }

    pub(crate) fn take(&mut self, node: &NodeId) -> Option<Vec<String>> {
        self.inner.remove(node)
    }

    pub(crate) fn remove(&mut self, node: &NodeId) -> Option<Vec<String>> {
        self.inner.remove(node)
    }

    pub(crate) fn clear(&mut self) {
        self.inner.clear();
    }

    #[cfg(test)]
    pub(crate) fn len_for(&self, node: &NodeId) -> usize {
        self.inner.get(node).map_or(0, Vec::len)
    }
}

pub(crate) fn is_active_for_pending(state: Option<&ConnectionState>) -> bool {
    matches!(
        state,
        Some(ConnectionState::Connecting)
            | Some(ConnectionState::Connected)
            | Some(ConnectionState::Reconnecting)
    )
}

pub(crate) fn should_buffer_candidate(
    state: Option<&ConnectionState>,
    buffer_early_candidates: bool,
) -> bool {
    is_active_for_pending(state) || (buffer_early_candidates && state.is_none())
}
