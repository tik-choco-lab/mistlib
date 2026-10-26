//! Application callback codes shared by every engine. These are not wire protocol tags.

/// Expands the canonical application event table. Kept as literals here because
/// wasm-bindgen requires literal enum discriminants when generating JS exports.
#[macro_export]
macro_rules! application_event_codes {
    ($consumer:ident) => {
        $consumer! {
            RAW, EVENT_RAW, Raw, 0;
            OVERLAY, EVENT_OVERLAY, Overlay, 1;
            PEER_CONNECTED, EVENT_PEER_CONNECTED, PeerConnected, 2;
            PEER_DISCONNECTED, EVENT_PEER_DISCONNECTED, PeerDisconnected, 3;
            NEIGHBORS, EVENT_NEIGHBORS, Neighbors, 4;
            AOI_ENTERED, EVENT_AOI_ENTERED, AoiEntered, 5;
            AOI_LEFT, EVENT_AOI_LEFT, AoiLeft, 6;
            NODE_POSITION_UPDATED, EVENT_NODE_POSITION_UPDATED, NodePositionUpdated, 7;
            AOI_NODES, EVENT_AOI_NODES, AoiNodes, 8;
            ALL_CONNECTIONS_LOST, EVENT_ALL_CONNECTIONS_LOST, AllConnectionsLost, 9;
            ROOM_JOINED, EVENT_ROOM_JOINED, RoomJoined, 10;
            ROOM_JOIN_FAILED, EVENT_ROOM_JOIN_FAILED, RoomJoinFailed, 11;
            ROOM_LEFT, EVENT_ROOM_LEFT, RoomLeft, 12;
        }
    };
}

/// Media callbacks have a separate payload/API, but share the reserved code space.
#[macro_export]
macro_rules! media_event_codes {
    ($consumer:ident) => {
        $consumer! {
            TRACK_ADDED, MEDIA_EVENT_TRACK_ADDED, TrackAdded, 100;
            TRACK_REMOVED, MEDIA_EVENT_TRACK_REMOVED, TrackRemoved, 101;
        }
    };
}

macro_rules! define_codes {
    ($($name:ident, $constant:ident, $variant:ident, $code:literal;)*) => {
        $(pub const $constant: u32 = $code;)*
    };
}
application_event_codes!(define_codes);
media_event_codes!(define_codes);

/// Tracks the per-session transition from some connected peers to none.
/// Reconnection arms the next loss notification; an initially empty room is quiet.
#[derive(Default)]
pub struct ConnectionLossTracker(std::sync::atomic::AtomicBool);

impl ConnectionLossTracker {
    pub fn update(&self, connected_count: usize) -> bool {
        use std::sync::atomic::Ordering;
        if connected_count > 0 {
            self.0.store(true, Ordering::Relaxed);
            false
        } else {
            self.0.swap(false, Ordering::Relaxed)
        }
    }

    pub fn reset(&self) {
        self.0.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

pub const ALL_CONNECTIONS_LOST_PAYLOAD: &[u8] =
    br#"{"reason":"allConnectionsLost","connectedCount":0}"#;

pub fn position_payload(x: f32, y: f32, z: f32) -> Vec<u8> {
    format!("{{\"x\":{}, \"y\":{}, \"z\":{}}}", x, y, z).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_callback_table_is_pinned() {
        macro_rules! table {
            ($($name:ident, $constant:ident, $variant:ident, $code:literal;)*) => {
                &[$(($constant, stringify!($name)),)*]
            };
        }
        let application: &[(u32, &str)] = application_event_codes!(table);
        let media: &[(u32, &str)] = media_event_codes!(table);
        let actual: Vec<_> = application.iter().chain(media).copied().collect();
        assert_eq!(
            actual,
            [
                (0, "RAW"),
                (1, "OVERLAY"),
                (2, "PEER_CONNECTED"),
                (3, "PEER_DISCONNECTED"),
                (4, "NEIGHBORS"),
                (5, "AOI_ENTERED"),
                (6, "AOI_LEFT"),
                (7, "NODE_POSITION_UPDATED"),
                (8, "AOI_NODES"),
                (9, "ALL_CONNECTIONS_LOST"),
                (10, "ROOM_JOINED"),
                (11, "ROOM_JOIN_FAILED"),
                (12, "ROOM_LEFT"),
                (100, "TRACK_ADDED"),
                (101, "TRACK_REMOVED"),
            ]
        );
    }

    #[test]
    fn connection_loss_is_once_per_transition_and_resets_per_session() {
        let tracker = ConnectionLossTracker::default();
        for (count, expected) in [
            (0, false),
            (2, false),
            (1, false),
            (0, true),
            (0, false),
            (1, false),
            (0, true),
        ] {
            assert_eq!(tracker.update(count), expected);
        }
        tracker.update(1);
        tracker.reset();
        assert!(!tracker.update(0));
    }
}
