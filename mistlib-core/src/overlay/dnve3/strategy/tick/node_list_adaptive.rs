use crate::overlay::dnve3::spatial_density::Vector3;
use crate::types::NodeId;
use std::collections::HashSet;
use std::sync::Mutex;

/// How many consecutive quiet rounds (no new/changed nodes) it takes before the
/// node_list interval is stretched by one more step.
const STALE_ROUNDS_TO_EXTEND: u32 = 3;

/// Node_list interval never backs off past this multiple of the configured base
/// interval, so a long-idle room still refreshes at a bounded worst-case rate.
const MAX_INTERVAL_MULTIPLIER: f32 = 4.0;

struct AdaptiveState {
    initialized: bool,
    multiplier: f32,
    stale_rounds: u32,
    known_ids: HashSet<NodeId>,
    connected_count: usize,
    reset_position: Vector3,
}

impl AdaptiveState {
    fn new() -> Self {
        Self {
            initialized: false,
            multiplier: 1.0,
            stale_rounds: 0,
            known_ids: HashSet::new(),
            connected_count: 0,
            reset_position: Vector3::zero(),
        }
    }
}

/// Tracks whether recent node_list rounds have surfaced anything new, and grows
/// the effective node_list interval (as a multiplier on the configured base) when
/// a room has settled. Any topology change (new node, connection count change, or
/// a large self movement) snaps the multiplier back to the base interval.
pub(crate) struct NodeListAdaptiveTracker {
    state: Mutex<AdaptiveState>,
}

impl NodeListAdaptiveTracker {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(AdaptiveState::new()),
        }
    }

    /// The multiplier to apply to the configured base interval for the *next*
    /// due-time computation. Reflects bookkeeping from the last `observe` call.
    pub(crate) fn current_multiplier(&self) -> f32 {
        self.state
            .lock()
            .expect("node_list adaptive state lock poisoned")
            .multiplier
    }

    /// Called once per actual node_list round (i.e. when the tick was due) with
    /// the freshly observed world state, to decide whether this was a "quiet"
    /// round or a change that should reset the backoff.
    pub(crate) fn observe(
        &self,
        self_pos: Vector3,
        connected_nodes: &[NodeId],
        known_ids: HashSet<NodeId>,
        aoi_range: f32,
    ) {
        let mut state = self
            .state
            .lock()
            .expect("node_list adaptive state lock poisoned");

        if !state.initialized {
            state.initialized = true;
            state.known_ids = known_ids;
            state.connected_count = connected_nodes.len();
            state.reset_position = self_pos;
            return;
        }

        let membership_changed = known_ids != state.known_ids;
        let count_changed = connected_nodes.len() != state.connected_count;
        let move_threshold = (aoi_range / 4.0).max(0.0);
        let moved_far = self_pos.dist(state.reset_position) >= move_threshold;

        if membership_changed || count_changed || moved_far {
            state.stale_rounds = 0;
            state.multiplier = 1.0;
            state.reset_position = self_pos;
        } else {
            state.stale_rounds += 1;
            if state.stale_rounds >= STALE_ROUNDS_TO_EXTEND {
                state.stale_rounds = 0;
                state.multiplier = (state.multiplier * 2.0).min(MAX_INTERVAL_MULTIPLIER);
            }
        }

        state.known_ids = known_ids;
        state.connected_count = connected_nodes.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> NodeId {
        NodeId(id.to_string())
    }

    fn ids(names: &[&str]) -> HashSet<NodeId> {
        names.iter().map(|n| node(n)).collect()
    }

    #[test]
    fn first_observe_establishes_baseline_without_growing() {
        let tracker = NodeListAdaptiveTracker::new();
        assert_eq!(tracker.current_multiplier(), 1.0);

        tracker.observe(Vector3::zero(), &[node("a")], ids(&["a"]), 10.0);
        assert_eq!(
            tracker.current_multiplier(),
            1.0,
            "the first observation should not count as a stale round"
        );
    }

    #[test]
    fn quiet_rounds_grow_the_multiplier_up_to_the_cap() {
        let tracker = NodeListAdaptiveTracker::new();
        tracker.observe(Vector3::zero(), &[node("a")], ids(&["a"]), 10.0);

        // 3 quiet rounds -> first doubling.
        for _ in 0..3 {
            tracker.observe(Vector3::zero(), &[node("a")], ids(&["a"]), 10.0);
        }
        assert_eq!(tracker.current_multiplier(), 2.0);

        // 3 more quiet rounds -> doubles again, hitting the cap.
        for _ in 0..3 {
            tracker.observe(Vector3::zero(), &[node("a")], ids(&["a"]), 10.0);
        }
        assert_eq!(tracker.current_multiplier(), 4.0);

        // Further quiet rounds must not exceed the cap.
        for _ in 0..6 {
            tracker.observe(Vector3::zero(), &[node("a")], ids(&["a"]), 10.0);
        }
        assert_eq!(tracker.current_multiplier(), MAX_INTERVAL_MULTIPLIER);
    }

    #[test]
    fn new_node_resets_multiplier_to_base() {
        let tracker = NodeListAdaptiveTracker::new();
        tracker.observe(Vector3::zero(), &[node("a")], ids(&["a"]), 10.0);
        for _ in 0..3 {
            tracker.observe(Vector3::zero(), &[node("a")], ids(&["a"]), 10.0);
        }
        assert_eq!(tracker.current_multiplier(), 2.0);

        tracker.observe(
            Vector3::zero(),
            &[node("a"), node("b")],
            ids(&["a", "b"]),
            10.0,
        );
        assert_eq!(
            tracker.current_multiplier(),
            1.0,
            "a newly discovered node must reset the interval to the base"
        );
    }

    #[test]
    fn connected_count_change_resets_multiplier_even_with_same_known_ids() {
        let tracker = NodeListAdaptiveTracker::new();
        tracker.observe(
            Vector3::zero(),
            &[node("a"), node("b")],
            ids(&["a", "b"]),
            10.0,
        );
        for _ in 0..3 {
            tracker.observe(
                Vector3::zero(),
                &[node("a"), node("b")],
                ids(&["a", "b"]),
                10.0,
            );
        }
        assert_eq!(tracker.current_multiplier(), 2.0);

        // "b" drops off the connected list but is still known (e.g. one hop away).
        tracker.observe(Vector3::zero(), &[node("a")], ids(&["a", "b"]), 10.0);
        assert_eq!(
            tracker.current_multiplier(),
            1.0,
            "a connected-count change must reset the interval even if known ids are unchanged"
        );
    }

    #[test]
    fn large_self_movement_resets_multiplier() {
        let tracker = NodeListAdaptiveTracker::new();
        tracker.observe(Vector3::zero(), &[node("a")], ids(&["a"]), 10.0);
        for _ in 0..3 {
            tracker.observe(Vector3::zero(), &[node("a")], ids(&["a"]), 10.0);
        }
        assert_eq!(tracker.current_multiplier(), 2.0);

        // aoi_range = 10.0 -> reset threshold is 2.5; move further than that.
        tracker.observe(Vector3::new(3.0, 0.0, 0.0), &[node("a")], ids(&["a"]), 10.0);
        assert_eq!(
            tracker.current_multiplier(),
            1.0,
            "moving more than aoi_range/4 must reset the interval to the base"
        );
    }

    #[test]
    fn small_self_movement_does_not_reset_multiplier() {
        let tracker = NodeListAdaptiveTracker::new();
        tracker.observe(Vector3::zero(), &[node("a")], ids(&["a"]), 10.0);
        for _ in 0..3 {
            tracker.observe(Vector3::new(0.1, 0.0, 0.0), &[node("a")], ids(&["a"]), 10.0);
        }
        assert_eq!(
            tracker.current_multiplier(),
            2.0,
            "small jitter movement under aoi_range/4 should not block the backoff"
        );
    }
}
