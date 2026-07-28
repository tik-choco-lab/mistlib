//! Centralized wire (de)serialization for `OverlayEnvelope` frames (Envelope v4).
//!
//! Envelope v4 switches the on-wire integer encoding from bincode's **fixint**
//! form (used by the `bincode::serialize` / `bincode::deserialize` free
//! functions: every length prefix and enum discriminant is a fixed 8/4 bytes)
//! to bincode's **varint** form via [`bincode::DefaultOptions`]. For a typical
//! frame this shrinks:
//!
//! - each `NodeId` string length prefix: 8 → 1 byte
//! - the `MessageContent` enum tag: 4 → 1 byte
//! - `hop_count` and the common `seq == 0` case: 4/8 → 1 byte
//! - the `Raw`/payload `Vec<u8>` length prefix: 8 → 1 byte
//!
//! cutting ~33–40 bytes off a typical envelope (e.g. a position-sync frame goes
//! from ~100 to ~64 bytes). `msg_id` stays a full-entropy `u64` on purpose — it
//! costs ~9 varint bytes for a random value, but narrowing its entropy would
//! raise the duplicate-suppression collision probability on `ReliableOrdered`
//! streams, which is not worth 4 bytes.
//!
//! This is a **wire-incompatible** change: every node (native and wasm binaries
//! embedding `mistlib-core`) must be upgraded together. Wrappers (Unity/Python/
//! JS/Go) are insulated — they only exchange opaque payloads across the FFI, so
//! they need no change.
//!
//! `DefaultOptions` also **rejects trailing bytes** (the free functions allow
//! them). Every envelope frame is exactly one encoded value, and the two
//! decode-then-fallback multiplexers (`engine/network.rs` in core and native)
//! actively *want* non-envelope bytes to fail cleanly, so rejecting trailing
//! bytes is strictly safer here.
//!
//! IMPORTANT: route **only** `OverlayEnvelope` frames through this module.
//! Inner payloads carried inside `OverlayMessage.payload` / `MessageContent::Raw`
//! (e.g. a `Vector3` position, node lists, density payloads) keep the fixint
//! free-function encoding and must NOT be sent through here — mixing the two
//! would corrupt those nested blobs.

use bincode::Options;
use serde::de::DeserializeOwned;
use serde::Serialize;

/// The single source of truth for the envelope wire encoding: varint integers,
/// little-endian, reject-trailing (bincode's `DefaultOptions`).
fn options() -> impl Options {
    bincode::DefaultOptions::new()
}

/// Serializes an `OverlayEnvelope` (or any envelope-level value) to bytes using
/// the Envelope v4 varint encoding.
pub fn serialize<T: Serialize + ?Sized>(value: &T) -> bincode::Result<Vec<u8>> {
    options().serialize(value)
}

/// Deserializes bytes produced by [`serialize`] back into an envelope value.
/// Returns `Err` (rejecting trailing bytes) for any input that is not exactly
/// one v4-encoded value — callers rely on this to fall back to raw/storage
/// handling.
pub fn deserialize<T: DeserializeOwned>(bytes: &[u8]) -> bincode::Result<T> {
    options().deserialize(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::message::{OverlayEnvelope, OverlayMessage};
    use crate::signaling::MessageContent;
    use crate::types::NodeId;

    fn sample_position_sync() -> OverlayEnvelope {
        // Mirrors wasm `update_position`: a 12-byte bincode `Vector3` payload
        // (0.0/0.0/0.0 → 12 zero bytes) wrapped as an Overlay message type 200,
        // broadcast from a UUID-shaped sender.
        let payload = vec![0u8; 12];
        OverlayEnvelope::new(
            NodeId("550e8400-e29b-41d4-a716-446655440000".to_string()),
            NodeId::broadcast(),
            0,
            MessageContent::Overlay(OverlayMessage {
                message_type: 200,
                payload,
            }),
        )
    }

    #[test]
    fn envelope_round_trips_through_varint_wire() {
        let env = sample_position_sync();
        let bytes = serialize(&env).expect("serialize");
        let decoded: OverlayEnvelope = deserialize(&bytes).expect("deserialize");
        assert_eq!(decoded.from, env.from);
        assert_eq!(decoded.to, env.to);
        assert_eq!(decoded.msg_id, env.msg_id);
        assert_eq!(decoded.seq, env.seq);
        match decoded.content {
            MessageContent::Overlay(m) => {
                assert_eq!(m.message_type, 200);
                assert_eq!(m.payload, vec![0u8; 12]);
            }
            other => panic!("unexpected content variant: {other:?}"),
        }
    }

    #[test]
    fn varint_wire_is_much_smaller_than_fixint_free_functions() {
        let env = sample_position_sync();
        let v4 = serialize(&env).expect("v4 serialize");
        let legacy = bincode::serialize(&env).expect("legacy serialize");
        // The whole point of Envelope v4: the varint frame must be materially
        // smaller than the old fixint free-function frame. For this position-sync
        // shape the saving is ~35 bytes; assert a conservative lower bound so the
        // test documents the win without being brittle to msg_id varint width.
        assert!(
            v4.len() + 20 <= legacy.len(),
            "expected v4 ({}) to be >=20 bytes smaller than legacy ({})",
            v4.len(),
            legacy.len()
        );
    }

    #[test]
    #[ignore = "measurement only; run with --ignored --nocapture"]
    fn measure_wire_savings() {
        use crate::overlay::message::{OVERLAY_MSG_HEARTBEAT, OVERLAY_MSG_PING};
        let uuid = || NodeId("550e8400-e29b-41d4-a716-446655440000".to_string());
        let cases: Vec<(&str, OverlayEnvelope)> = vec![
            ("position-sync (Vector3 broadcast)", sample_position_sync()),
            (
                "ping (empty overlay unicast)",
                OverlayEnvelope::new(
                    uuid(),
                    uuid(),
                    0,
                    MessageContent::Overlay(OverlayMessage {
                        message_type: OVERLAY_MSG_PING,
                        payload: vec![],
                    }),
                ),
            ),
            (
                "heartbeat (32B payload broadcast)",
                OverlayEnvelope::new(
                    uuid(),
                    NodeId::broadcast(),
                    0,
                    MessageContent::Overlay(OverlayMessage {
                        message_type: OVERLAY_MSG_HEARTBEAT,
                        payload: vec![0u8; 32],
                    }),
                ),
            ),
            (
                "raw chat (16B unicast, seq=5)",
                OverlayEnvelope::new(uuid(), uuid(), 0, MessageContent::Raw(vec![0u8; 16].into()))
                    .with_seq(5),
            ),
        ];
        for (name, env) in cases {
            let v4 = serialize(&env).unwrap().len();
            let legacy = bincode::serialize(&env).unwrap().len();
            println!(
                "{name}: legacy={legacy}B  v4={v4}B  saved={}B ({:.0}%)",
                legacy - v4,
                100.0 * (legacy - v4) as f64 / legacy as f64
            );
        }
    }

    #[test]
    fn deserialize_rejects_trailing_bytes() {
        let env = sample_position_sync();
        let mut bytes = serialize(&env).expect("serialize");
        bytes.push(0xFF); // one extra byte → must not decode as a clean envelope
        assert!(deserialize::<OverlayEnvelope>(&bytes).is_err());
    }
}
