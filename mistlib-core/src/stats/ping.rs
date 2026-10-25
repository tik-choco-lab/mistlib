use crate::action::OverlayAction;
use crate::overlay::{OverlayEnvelope, OverlayMessage, OVERLAY_MSG_PING, OVERLAY_MSG_PONG};
use crate::signaling::MessageContent;
use crate::stats::STATS;
use crate::types::{DeliveryMethod, NodeId};
use std::sync::OnceLock;
use web_time::Instant;

fn monotonic_micros() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_micros() as u64
}

fn send_ping_all(
    local_node_id: &NodeId,
    hop_count: u32,
    connected_nodes: &[NodeId],
) -> Vec<OverlayAction> {
    let now = monotonic_micros();
    let payload = now.to_le_bytes().to_vec();

    connected_nodes
        .iter()
        .filter_map(|target| {
            let envelope = OverlayEnvelope::new(
                local_node_id.clone(),
                target.clone(),
                hop_count,
                MessageContent::Overlay(OverlayMessage {
                    message_type: OVERLAY_MSG_PING,
                    payload: payload.clone(),
                }),
            );

            let data = crate::overlay::wire::serialize(&envelope)
                .map_err(|e| {
                    tracing::warn!("[Stats] failed to serialize ping to {}: {}", target, e)
                })
                .ok()?;
            Some(OverlayAction::SendMessage {
                to: target.clone(),
                data: bytes::Bytes::from(data),
                method: DeliveryMethod::Unreliable,
                lane: crate::action::SendLane::Control,
            })
        })
        .collect()
}

pub(crate) fn tick_actions(
    local_node_id: &NodeId,
    hop_count: u32,
    connected_nodes: &[NodeId],
    timeout_count: u32,
) -> Vec<OverlayAction> {
    let mut actions = Vec::new();

    for target in connected_nodes {
        let misses = STATS.note_ping_sent(target, timeout_count);
        // Edge-triggered: `misses` only ever equals `timeout_count` on the tick
        // it first crosses the threshold (it keeps climbing past it on every
        // later miss), so this fires exactly once per suspect episode until an
        // inbound frame resets the streak.
        if timeout_count > 0 && misses == timeout_count {
            tracing::warn!(
                "[Stats] peer {} had {} consecutive ping rounds without inbound activity; marking as disconnect-suspect \
                 so the transport's reconnect-grace flow can confirm or clear it",
                target,
                misses
            );
            actions.push(OverlayAction::SuspectDisconnected { to: target.clone() });
        }
    }

    actions.extend(send_ping_all(local_node_id, hop_count, connected_nodes));
    actions
}

pub(crate) fn handle_ping(
    local_node_id: &NodeId,
    from: NodeId,
    hop_count: u32,
    payload: &[u8],
) -> Vec<OverlayAction> {
    let envelope = OverlayEnvelope::new(
        local_node_id.clone(),
        from.clone(),
        hop_count,
        MessageContent::Overlay(OverlayMessage {
            message_type: OVERLAY_MSG_PONG,
            payload: payload.to_vec(),
        }),
    );

    match crate::overlay::wire::serialize(&envelope) {
        Ok(data) => vec![OverlayAction::SendMessage {
            to: from,
            data: bytes::Bytes::from(data),
            method: DeliveryMethod::Unreliable,
            lane: crate::action::SendLane::Control,
        }],
        Err(e) => {
            tracing::warn!("[Stats] failed to serialize pong envelope: {}", e);
            vec![]
        }
    }
}

pub(crate) fn handle_pong(from: NodeId, payload: &[u8]) -> Vec<OverlayAction> {
    if payload.len() < 8 {
        return Vec::new();
    }
    let sent_time = u64::from_le_bytes(payload[..8].try_into().unwrap_or_default());
    let now = monotonic_micros();
    let rtt_ms = now.saturating_sub(sent_time) as f32 / 1000.0;
    let was_suspect = STATS.note_pong_received(&from);
    STATS.set_rtt(from.clone(), rtt_ms);

    if was_suspect {
        vec![OverlayAction::ClearSuspect { to: from }]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::OVERLAY_MSG_PONG;

    fn node(id: &str) -> NodeId {
        NodeId(id.to_string())
    }

    #[test]
    fn transport_bytes_reset_misses_and_clear_suspect_only_on_growth() {
        use crate::stats::{MistStats, TransportReceiveCounter};
        let stats = MistStats::new();
        let peer = node("byte-growth");
        let mut counter = TransportReceiveCounter::default();
        assert!(!counter.observe("transport", 100));
        for _ in 0..=3 {
            stats.note_ping_sent(&peer, 3);
        }
        assert_eq!(stats.ping_consecutive_misses(&peer), 3);
        assert!(!counter.observe("transport", 100));
        assert_eq!(stats.note_ping_sent(&peer, 3), 4);
        assert!(counter.observe("transport", 101));
        assert!(stats.note_inbound_activity(&peer));
        assert_eq!(stats.ping_consecutive_misses(&peer), 0);
        assert_eq!(stats.note_ping_sent(&peer, 3), 0);
        // A new counter, reset counter, or changed selected pair is a baseline.
        assert!(!counter.observe("transport", 1));
        assert!(!counter.observe("other-pair", 1000));
        assert!(!TransportReceiveCounter::default().observe("transport", 1000));
        assert_eq!(stats.note_ping_sent(&peer, 3), 1);
        assert!(counter.observe("other-pair", 1001));
        assert!(!stats.note_inbound_activity(&peer));
    }

    #[test]
    fn handle_ping_returns_pong_action() {
        let payload = 12345u64.to_le_bytes().to_vec();
        let actions = handle_ping(&node("local"), node("sender"), 1, &payload);
        assert_eq!(
            actions.len(),
            1,
            "handle_ping should return one pong action"
        );

        let OverlayAction::SendMessage { data, .. } = &actions[0] else {
            panic!("pong action should be SendMessage");
        };
        let envelope: OverlayEnvelope =
            crate::overlay::wire::deserialize(data).expect("pong data should be a valid envelope");
        let MessageContent::Overlay(msg) = &envelope.content else {
            panic!("pong message should be an overlay message");
        };
        assert_eq!(msg.message_type, OVERLAY_MSG_PONG);
        assert_eq!(&msg.payload, &payload, "pong payload should echo ping");
    }

    #[test]
    fn handle_pong_short_payload_is_noop() {
        let actions = handle_pong(node("peer"), b"short");
        assert!(actions.is_empty());
    }

    // These tests drive the process-wide STATS singleton (ping.rs isn't
    // parameterized over a MistStats instance), so each test uses a NodeId no
    // other test touches to stay independent under parallel test execution.

    #[test]
    fn tick_actions_first_round_is_never_a_miss() {
        let local = node("ping-test-local-a");
        let peer = node("ping-test-peer-a");
        tick_actions(&local, 1, std::slice::from_ref(&peer), 5);
        assert_eq!(STATS.ping_consecutive_misses(&peer), 0);
    }

    #[test]
    fn tick_actions_counts_consecutive_misses_when_no_pong_arrives() {
        let local = node("ping-test-local-b");
        let peer = node("ping-test-peer-b");

        tick_actions(&local, 1, std::slice::from_ref(&peer), 5);
        assert_eq!(
            STATS.ping_consecutive_misses(&peer),
            0,
            "first round is a baseline, not a miss"
        );

        tick_actions(&local, 1, std::slice::from_ref(&peer), 5);
        assert_eq!(STATS.ping_consecutive_misses(&peer), 1);

        tick_actions(&local, 1, std::slice::from_ref(&peer), 5);
        assert_eq!(STATS.ping_consecutive_misses(&peer), 2);
    }

    #[test]
    fn handle_pong_resets_the_miss_streak() {
        let local = node("ping-test-local-c");
        let peer = node("ping-test-peer-c");

        tick_actions(&local, 1, std::slice::from_ref(&peer), 5);
        tick_actions(&local, 1, std::slice::from_ref(&peer), 5);
        assert_eq!(STATS.ping_consecutive_misses(&peer), 1);

        let payload = 42u64.to_le_bytes().to_vec();
        let actions = handle_pong(peer.clone(), &payload);
        assert_eq!(STATS.ping_consecutive_misses(&peer), 0);
        assert!(
            actions.is_empty(),
            "a miss streak below the threshold must not emit ClearSuspect"
        );

        // The round right after a pong must not itself be counted as a miss.
        tick_actions(&local, 1, std::slice::from_ref(&peer), 5);
        assert_eq!(STATS.ping_consecutive_misses(&peer), 0);
    }

    #[test]
    fn tick_actions_with_timeout_count_zero_still_tracks_but_never_warns_specially() {
        let local = node("ping-test-local-d");
        let peer = node("ping-test-peer-d");

        for _ in 0..7 {
            let actions = tick_actions(&local, 1, std::slice::from_ref(&peer), 0);
            assert!(
                !actions
                    .iter()
                    .any(|a| matches!(a, OverlayAction::SuspectDisconnected { .. })),
                "timeout_count=0 must never emit SuspectDisconnected"
            );
        }
        // Tracking keeps counting even when the threshold check is disabled;
        // only the warn-log side effect and SuspectDisconnected are skipped.
        assert_eq!(STATS.ping_consecutive_misses(&peer), 6);
    }

    #[test]
    fn tick_actions_emits_suspect_disconnected_exactly_once_at_threshold() {
        let local = node("ping-test-local-e");
        let peer = node("ping-test-peer-e");
        let timeout_count = 3;

        let mut suspect_emissions = 0;
        for _ in 0..(timeout_count + 4) {
            let actions = tick_actions(&local, 1, std::slice::from_ref(&peer), timeout_count);
            suspect_emissions += actions
                .iter()
                .filter(|a| matches!(a, OverlayAction::SuspectDisconnected { to } if *to == peer))
                .count();
        }

        assert_eq!(
            suspect_emissions, 1,
            "SuspectDisconnected must latch: exactly one emission per suspect episode, \
             even though misses keep climbing past the threshold"
        );
    }

    #[test]
    fn inbound_activity_prevents_suspicion_without_pongs() {
        let local = node("ping-activity-local");
        let peer = node("ping-activity-peer");
        let peers = std::slice::from_ref(&peer);
        let threshold = 3;
        for _ in 0..threshold {
            tick_actions(&local, 1, peers, threshold);
        }
        assert_eq!(STATS.ping_consecutive_misses(&peer), threshold - 1);
        assert!(!STATS.note_inbound_activity(&peer));
        assert_eq!(STATS.ping_consecutive_misses(&peer), 0);
        for _ in 0..20 {
            let actions = tick_actions(&local, 1, peers, threshold);
            assert!(!actions
                .iter()
                .any(|a| matches!(a, OverlayAction::SuspectDisconnected { .. })));
            assert_eq!(STATS.ping_consecutive_misses(&peer), 0);
            assert!(!STATS.note_inbound_activity(&peer));
        }
        // Silence after sustained data must still start a fresh suspect episode.
        let mut suspects = 0;
        for _ in 0..threshold + 4 {
            suspects += tick_actions(&local, 1, peers, threshold)
                .iter()
                .filter(|a| matches!(a, OverlayAction::SuspectDisconnected { .. }))
                .count();
        }
        assert_eq!(suspects, 1);
    }

    #[test]
    fn handle_pong_clears_suspect_after_threshold_is_crossed() {
        let local = node("ping-test-local-f");
        let peer = node("ping-test-peer-f");
        let timeout_count = 3;

        let mut saw_suspect = false;
        // The first tick_actions call is a baseline (never a miss), so it takes
        // timeout_count + 1 calls with no PONG in between to cross the threshold.
        for _ in 0..=timeout_count {
            let actions = tick_actions(&local, 1, std::slice::from_ref(&peer), timeout_count);
            saw_suspect |= actions
                .iter()
                .any(|a| matches!(a, OverlayAction::SuspectDisconnected { to } if *to == peer));
        }
        assert!(saw_suspect, "test setup should have crossed the threshold");

        let payload = 7u64.to_le_bytes().to_vec();
        let actions = handle_pong(peer.clone(), &payload);
        assert_eq!(
            actions.len(),
            1,
            "a PONG recovering a latched-suspect peer must emit ClearSuspect"
        );
        assert!(matches!(
            &actions[0],
            OverlayAction::ClearSuspect { to } if *to == peer
        ));

        // A later PONG (peer already cleared) must not re-emit ClearSuspect.
        let actions = handle_pong(peer.clone(), &payload);
        assert!(actions.is_empty());
    }
}
