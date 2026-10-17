use crate::types::NodeId;
use std::collections::HashSet;

/// None allows every peer; an empty set allows nobody.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackAudience(Option<HashSet<NodeId>>);

impl TrackAudience {
    pub fn new(nodes: Option<Vec<NodeId>>) -> Self {
        Self(nodes.map(|nodes| nodes.into_iter().collect()))
    }

    pub fn allows(&self, node: &NodeId) -> bool {
        self.0.as_ref().is_none_or(|nodes| nodes.contains(node))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> NodeId {
        NodeId(id.to_string())
    }

    #[test]
    fn default_allows_every_peer() {
        let audience = TrackAudience::default();
        assert!(audience.allows(&node("a")));
        assert_eq!(audience, TrackAudience::new(None));
    }

    #[test]
    fn empty_allows_nobody() {
        assert!(!TrackAudience::new(Some(vec![])).allows(&node("a")));
    }

    #[test]
    fn listed_peers_only() {
        let audience = TrackAudience::new(Some(vec![node("a"), node("a"), node("b")]));
        assert!(audience.allows(&node("a")));
        assert!(audience.allows(&node("b")));
        assert!(!audience.allows(&node("c")));
    }
}
