use super::codec::{DecodedDiscovery, NostrCodecConfig};
use super::dedupe::DedupeCache;
use super::identity::DiscoveryTable;
use crate::signaling::{SignalingData, SignalingType};
use std::collections::HashMap;

pub const DEFAULT_MAX_DISCOVERY_RESPONDERS_PER_PEER: usize = 2;

/// Bounded reorder tolerance for `accept_message_order`.
///
/// A message whose sequence trails the per-sender high-water mark by fewer
/// than this many positions is still accepted (without moving the mark)
/// instead of being treated as stale. This absorbs two sources of legitimate
/// reordering that are not attacks: the sender-side race where a
/// small/cheap message (e.g. a trickled ICE candidate) can win the wire race
/// against a message that was assigned an earlier sequence but is still
/// doing CPU-bound crypto, and, looking ahead, delivery across multiple
/// relay connections where cross-relay ordering is not guaranteed at all.
/// Replay safety does not depend on this window: every message must first
/// pass the event-id dedupe cache, so a message beyond the window is
/// rejected as `StaleSequence` only because it is old, not because it could
/// be a replay.
pub const NOSTR_SEQUENCE_REORDER_WINDOW: u64 = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageOrderAcceptance {
    Accepted,
    DuplicateMessageId,
    /// Sequence is at or behind the high-water mark but within
    /// `NOSTR_SEQUENCE_REORDER_WINDOW` of it: accepted, high-water mark
    /// unchanged.
    ReorderedWithinWindow {
        last: u64,
        sequence: u64,
    },
    /// Sequence is behind the high-water mark by at least
    /// `NOSTR_SEQUENCE_REORDER_WINDOW`: rejected as too old to be legitimate
    /// reorder.
    StaleSequence {
        last: u64,
        sequence: u64,
    },
    Gap {
        last: u64,
        sequence: u64,
    },
}

impl MessageOrderAcceptance {
    pub fn is_accepted(&self) -> bool {
        matches!(
            self,
            Self::Accepted | Self::Gap { .. } | Self::ReorderedWithinWindow { .. }
        )
    }
}

pub fn next_outgoing_sequence(
    outgoing_sequences: &mut HashMap<String, u64>,
    receiver_pubkey: &str,
) -> u64 {
    let sequence = outgoing_sequences
        .entry(receiver_pubkey.to_string())
        .or_insert(0);
    *sequence = sequence.saturating_add(1).max(1);
    *sequence
}

pub fn record_discovery_and_should_request(
    config: &NostrCodecConfig,
    discovery_table: &mut DiscoveryTable,
    decoded: &DecodedDiscovery,
    room_id: &str,
    local_pubkey: &str,
    max_responders: usize,
) -> bool {
    let local_rank = config.topology_rank(room_id, local_pubkey);
    discovery_table.insert_pubkey_with_rank(
        decoded.signaling_pubkey.clone(),
        decoded.expires_at,
        decoded.topology_rank.clone(),
    );
    discovery_table
        .responder_pubkeys_for(
            &decoded.signaling_pubkey,
            &decoded.topology_rank,
            local_pubkey,
            &local_rank,
            max_responders,
        )
        .iter()
        .any(|pubkey| pubkey == local_pubkey)
}

pub fn accept_sender_for_payload(
    discovery_table: &mut DiscoveryTable,
    sender_was_requested: bool,
    sender_pubkey: &str,
    data: &SignalingData,
) -> bool {
    if data.signaling_type == SignalingType::Request || sender_was_requested {
        return true;
    }
    discovery_table.pubkey_for_node(&data.sender_id).as_deref() == Some(sender_pubkey)
}

pub fn accept_message_order(
    message_dedupe: &mut DedupeCache,
    incoming_sequences: &mut HashMap<String, u64>,
    sender_pubkey: &str,
    message_id: Option<&str>,
    sequence: Option<u64>,
) -> MessageOrderAcceptance {
    if let Some(message_id) = message_id {
        if !message_dedupe.check_and_insert(message_id) {
            return MessageOrderAcceptance::DuplicateMessageId;
        }
    }

    let Some(sequence) = sequence else {
        return MessageOrderAcceptance::Accepted;
    };

    match incoming_sequences.get(sender_pubkey).copied() {
        Some(last) if sequence <= last => {
            if last - sequence < NOSTR_SEQUENCE_REORDER_WINDOW {
                MessageOrderAcceptance::ReorderedWithinWindow { last, sequence }
            } else {
                MessageOrderAcceptance::StaleSequence { last, sequence }
            }
        }
        Some(last) if sequence > last.saturating_add(1) => {
            incoming_sequences.insert(sender_pubkey.to_string(), sequence);
            MessageOrderAcceptance::Gap { last, sequence }
        }
        _ => {
            incoming_sequences.insert(sender_pubkey.to_string(), sequence);
            MessageOrderAcceptance::Accepted
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NostrSignalingConfig;
    use crate::signaling::nostr::util::now_unix_seconds;
    use crate::signaling::nostr::{DedupeCache, DiscoveryTable};
    use crate::signaling::{SignalingData, SignalingType};
    use crate::types::NodeId;
    use std::collections::HashMap;
    use web_time::Duration;

    fn data(sender_id: &str, signaling_type: SignalingType) -> SignalingData {
        SignalingData {
            sender_id: NodeId(sender_id.to_string()),
            receiver_id: NodeId("local".to_string()),
            room_id: "room".to_string(),
            data: String::new(),
            signaling_type,
        }
    }

    #[test]
    fn next_outgoing_sequence_is_per_receiver() {
        let mut sequences = HashMap::new();

        assert_eq!(next_outgoing_sequence(&mut sequences, "a"), 1);
        assert_eq!(next_outgoing_sequence(&mut sequences, "a"), 2);
        assert_eq!(next_outgoing_sequence(&mut sequences, "b"), 1);
    }

    #[test]
    fn sender_admission_allows_requests_and_known_or_requested_peers() {
        let mut table = DiscoveryTable::default();
        table.bind_node(
            NodeId("known".to_string()),
            "known-pubkey".to_string(),
            u64::MAX,
        );

        assert!(accept_sender_for_payload(
            &mut table,
            false,
            "unknown-pubkey",
            &data("unknown", SignalingType::Request),
        ));
        assert!(accept_sender_for_payload(
            &mut table,
            true,
            "requested-pubkey",
            &data("requested", SignalingType::Candidate),
        ));
        assert!(accept_sender_for_payload(
            &mut table,
            false,
            "known-pubkey",
            &data("known", SignalingType::Candidate),
        ));
        assert!(!accept_sender_for_payload(
            &mut table,
            false,
            "unknown-pubkey",
            &data("unknown", SignalingType::Candidate),
        ));
    }

    /// Reproduces the one-directional, silent failure `note_peer_alive`
    /// exists to prevent.
    ///
    /// A and B finish their handshake over the relay and then move to the
    /// overlay, so no more relay messages pass between them. B keeps
    /// accepting A's messages regardless -- B sent the original `Request`, and
    /// `sender_was_requested` is remembered for the whole session. A has only
    /// the `DiscoveryTable` binding, and with no periodic discovery to renew
    /// it, it lapses. The moment signaling has to fall back to the relay (ICE
    /// restart, reconnect after a blip) B -> A is rejected while A -> B still
    /// works.
    #[test]
    fn a_lapsed_binding_rejects_the_peer_that_never_sent_the_request() {
        let mut table = DiscoveryTable::default();
        // A's binding for B, as the handshake left it.
        table.bind_node(
            NodeId("peer-b".to_string()),
            "b-pubkey".to_string(),
            now_unix_seconds() + 1,
        );
        // B's side of the same pair: durable, because B sent the `Request`.
        assert!(accept_sender_for_payload(
            &mut DiscoveryTable::default(),
            true,
            "a-pubkey",
            &data("peer-a", SignalingType::Offer),
        ));

        std::thread::sleep(std::time::Duration::from_millis(1200));

        assert!(
            !accept_sender_for_payload(
                &mut table,
                false,
                "b-pubkey",
                &data("peer-b", SignalingType::Answer),
            ),
            "a lapsed binding is exactly the asymmetric failure: B -> A drops \
             while A -> B still succeeds"
        );
    }

    /// The fix: live traffic over any transport reaches
    /// `Signaler::note_peer_alive`, which renews the binding, so the relay
    /// fallback above is accepted in both directions.
    #[test]
    fn liveness_refresh_keeps_the_relay_fallback_working_in_both_directions() {
        let mut table = DiscoveryTable::default();
        table.bind_node(
            NodeId("peer-b".to_string()),
            "b-pubkey".to_string(),
            now_unix_seconds() + 1,
        );

        // Overlay traffic from B arrives; `RoutedSignalingHandler` reports it
        // and the Nostr signaler renews the binding.
        table.touch_node(&NodeId("peer-b".to_string()), 60);

        std::thread::sleep(std::time::Duration::from_millis(1200));

        assert!(
            accept_sender_for_payload(
                &mut table,
                false,
                "b-pubkey",
                &data("peer-b", SignalingType::Answer),
            ),
            "a peer kept alive over the overlay must still be admitted when \
             signaling falls back to the relay"
        );
    }

    #[test]
    fn discovery_request_decision_uses_ranked_limited_responders() {
        let raw = NostrSignalingConfig {
            relays: vec!["ws://127.0.0.1:7777".to_string()],
            relay_list_url: None,
            discovery_kind: 25049,
            message_kind: 25050,
            ttl_seconds: 60,
            max_clock_skew_seconds: 300,
            invite_salt: "salt".to_string(),
            invite_code: "invite".to_string(),
        };
        let codec = NostrCodecConfig::from_config(&raw);
        let room_id = "room";
        let mut pubkeys = (b'A'..=b'E')
            .map(|label| format!("pubkey-{}", label as char))
            .collect::<Vec<_>>();
        pubkeys.sort_by_key(|pubkey| (codec.topology_rank(room_id, pubkey), pubkey.clone()));
        let subject = pubkeys[3].clone();
        let decoded = DecodedDiscovery {
            signaling_pubkey: subject.clone(),
            expires_at: u64::MAX,
            topology_rank: codec.topology_rank(room_id, &subject),
            joined_at: None,
        };

        for (local_index, should_request) in [(2usize, true), (1usize, true), (0usize, false)] {
            let local = pubkeys[local_index].clone();
            let mut table = DiscoveryTable::default();
            for pubkey in &pubkeys {
                if pubkey != &local && pubkey != &subject {
                    table.insert_pubkey_with_rank(
                        pubkey.clone(),
                        u64::MAX,
                        codec.topology_rank(room_id, pubkey),
                    );
                }
            }

            assert_eq!(
                record_discovery_and_should_request(
                    &codec,
                    &mut table,
                    &decoded,
                    room_id,
                    &local,
                    DEFAULT_MAX_DISCOVERY_RESPONDERS_PER_PEER,
                ),
                should_request
            );
        }
    }

    #[test]
    fn message_order_rejects_replay_and_stale_sequence() {
        let mut dedupe = DedupeCache::new(Duration::from_secs(60));
        let mut sequences = HashMap::new();

        assert_eq!(
            accept_message_order(&mut dedupe, &mut sequences, "peer", Some("m1"), Some(1)),
            MessageOrderAcceptance::Accepted
        );
        assert_eq!(
            accept_message_order(&mut dedupe, &mut sequences, "peer", Some("m1"), Some(2)),
            MessageOrderAcceptance::DuplicateMessageId
        );
        assert_eq!(
            accept_message_order(&mut dedupe, &mut sequences, "peer", Some("m2"), Some(1)),
            MessageOrderAcceptance::ReorderedWithinWindow {
                last: 1,
                sequence: 1
            }
        );
        assert_eq!(
            accept_message_order(&mut dedupe, &mut sequences, "peer", Some("m3"), Some(3)),
            MessageOrderAcceptance::Gap {
                last: 1,
                sequence: 3
            }
        );
        assert_eq!(
            accept_message_order(
                &mut dedupe,
                &mut sequences,
                "other-peer",
                Some("m4"),
                Some(1),
            ),
            MessageOrderAcceptance::Accepted
        );
    }

    #[test]
    fn message_order_accepts_bounded_reorder_without_advancing_high_water() {
        let mut dedupe = DedupeCache::new(Duration::from_secs(60));
        let mut sequences = HashMap::new();

        assert_eq!(
            accept_message_order(&mut dedupe, &mut sequences, "peer", Some("seed"), Some(100)),
            MessageOrderAcceptance::Accepted
        );

        // Trails the high-water mark by less than the reorder window: accepted,
        // but the mark itself does not move backwards.
        let reordered_sequence = 100 - (NOSTR_SEQUENCE_REORDER_WINDOW - 1);
        assert_eq!(
            accept_message_order(
                &mut dedupe,
                &mut sequences,
                "peer",
                Some("reordered"),
                Some(reordered_sequence),
            ),
            MessageOrderAcceptance::ReorderedWithinWindow {
                last: 100,
                sequence: reordered_sequence
            }
        );
        assert_eq!(sequences.get("peer").copied(), Some(100));

        // A genuinely newer sequence still advances the mark afterwards.
        assert_eq!(
            accept_message_order(
                &mut dedupe,
                &mut sequences,
                "peer",
                Some("newer"),
                Some(101)
            ),
            MessageOrderAcceptance::Accepted
        );
        assert_eq!(sequences.get("peer").copied(), Some(101));
    }

    #[test]
    fn message_order_boundary_and_beyond_reorder_window_are_stale() {
        let mut dedupe = DedupeCache::new(Duration::from_secs(60));
        let mut sequences = HashMap::new();
        assert_eq!(
            accept_message_order(&mut dedupe, &mut sequences, "peer", Some("seed"), Some(200)),
            MessageOrderAcceptance::Accepted
        );

        // Exactly `NOSTR_SEQUENCE_REORDER_WINDOW` behind is outside the window
        // (the guard is `last - sequence < window`, so equality is stale).
        let at_boundary = 200 - NOSTR_SEQUENCE_REORDER_WINDOW;
        assert_eq!(
            accept_message_order(
                &mut dedupe,
                &mut sequences,
                "peer",
                Some("at-boundary"),
                Some(at_boundary),
            ),
            MessageOrderAcceptance::StaleSequence {
                last: 200,
                sequence: at_boundary
            }
        );

        // One position closer is inside the window.
        let just_inside = at_boundary + 1;
        assert_eq!(
            accept_message_order(
                &mut dedupe,
                &mut sequences,
                "peer",
                Some("just-inside"),
                Some(just_inside),
            ),
            MessageOrderAcceptance::ReorderedWithinWindow {
                last: 200,
                sequence: just_inside
            }
        );

        // Well beyond the window is stale too.
        assert_eq!(
            accept_message_order(
                &mut dedupe,
                &mut sequences,
                "peer",
                Some("ancient"),
                Some(1)
            ),
            MessageOrderAcceptance::StaleSequence {
                last: 200,
                sequence: 1
            }
        );
    }
}
