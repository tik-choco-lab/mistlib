//! Transport capability announcement (SPEC-25 §6.2).
//!
//! A peer may announce which link kinds it can carry, so both sides of a pair
//! can pick the same link deterministically. The announcement is optional on
//! the wire and absent by default; a peer that announces nothing (an older
//! build, a browser, or a native build with `webrtc.iceQuic: false`) is treated as
//! WebRTC-only.
//!
//! Two carriers, both optional and ignored by older readers:
//!
//! - Nostr discovery: an extra event tag `["transports", "webrtc", "iq1"]`
//!   (see [`crate::signaling::nostr::codec::TAG_TRANSPORTS`]).
//! - CONNECT `Request`: `SignalingData::data` holds the JSON
//!   `{"transports":["webrtc","iq1"]}` instead of the empty string. Readers
//!   that predate this treat any non-`"restart"` data as an ordinary
//!   connection request, and WASM never reads `Request` data.
//!
//! This module only encodes and decodes names. What a name means for link
//! selection is decided by the platform layer (native `LinkSelector`).

use serde::{Deserialize, Serialize};

/// The WebRTC link (every peer can carry it; announced for completeness).
pub const TRANSPORT_WEBRTC: &str = "webrtc";
/// The native ICE+QUIC link, version 1 (SPEC-25 §6.3).
pub const TRANSPORT_ICE_QUIC_V1: &str = "iq1";

/// Upper bound on names kept from one announcement. A Nostr tag holds at
/// most 8 fields, one of which is the tag name.
pub const MAX_ANNOUNCED_TRANSPORTS: usize = 7;
/// Names longer than this are dropped on decode (they cannot be a known
/// transport, and the bound keeps a hostile announcement cheap to hold).
pub const MAX_TRANSPORT_NAME_LEN: usize = 16;

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConnectRequestPayload {
    #[serde(default)]
    transports: Vec<String>,
}

/// Keeps at most [`MAX_ANNOUNCED_TRANSPORTS`] non-empty names no longer than
/// [`MAX_TRANSPORT_NAME_LEN`], in announcement order, without duplicates.
pub fn sanitize_transports<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if out.len() == MAX_ANNOUNCED_TRANSPORTS {
            break;
        }
        if name.is_empty() || name.len() > MAX_TRANSPORT_NAME_LEN {
            continue;
        }
        if !out.iter().any(|seen| seen == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// `SignalingData::data` for a CONNECT `Request`. Empty `transports` yields
/// the empty string, i.e. exactly what is sent when nothing is announced.
pub fn encode_connect_request_data(transports: &[&str]) -> String {
    if transports.is_empty() {
        return String::new();
    }
    let payload = ConnectRequestPayload {
        transports: sanitize_transports(transports.iter().copied()),
    };
    serde_json::to_string(&payload).unwrap_or_default()
}

/// Transports announced in a CONNECT `Request`'s data, or `None` when the
/// request announces nothing (empty data, the restart marker, or anything
/// that is not a JSON object). Unknown JSON fields are ignored.
pub fn decode_connect_request_transports(data: &str) -> Option<Vec<String>> {
    if !data.trim_start().starts_with('{') {
        return None;
    }
    let payload: ConnectRequestPayload = serde_json::from_str(data).ok()?;
    let names = sanitize_transports(payload.transports.iter().map(String::as_str));
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_announced_encodes_to_the_legacy_empty_data() {
        assert_eq!(encode_connect_request_data(&[]), "");
        assert_eq!(decode_connect_request_transports(""), None);
        assert_eq!(decode_connect_request_transports("restart"), None);
        assert_eq!(decode_connect_request_transports("not json"), None);
        assert_eq!(decode_connect_request_transports("{}"), None);
    }

    #[test]
    fn announced_transports_round_trip() {
        let data = encode_connect_request_data(&[TRANSPORT_WEBRTC, TRANSPORT_ICE_QUIC_V1]);
        assert_eq!(data, r#"{"transports":["webrtc","iq1"]}"#);
        assert_eq!(
            decode_connect_request_transports(&data),
            Some(vec!["webrtc".to_string(), "iq1".to_string()])
        );
    }

    #[test]
    fn decode_ignores_unknown_fields_and_bounds_names() {
        let long = "x".repeat(MAX_TRANSPORT_NAME_LEN + 1);
        let data = format!(
            r#"{{"future":1,"transports":["iq1","","iq1","{long}","a","b","c","d","e","f","g"]}}"#
        );
        let names = decode_connect_request_transports(&data).unwrap();
        assert_eq!(names.len(), MAX_ANNOUNCED_TRANSPORTS);
        assert_eq!(names[0], "iq1");
        assert!(!names.contains(&long));
        assert_eq!(names.iter().filter(|n| *n == "iq1").count(), 1);
    }

    /// The native/WASM readers that predate the announcement only compare
    /// Request data against the restart marker; a capability payload must
    /// never be mistaken for it.
    #[test]
    fn announcement_is_never_the_restart_marker() {
        let data = encode_connect_request_data(&[TRANSPORT_ICE_QUIC_V1]);
        assert_ne!(data, "restart");
    }
}
