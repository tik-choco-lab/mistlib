#[path = "../../src/app/events/storage_frame.rs"]
mod storage_frame;

use mistlib_core::engine::EngineEvent;
use mistlib_core::storage::cid::{compute_cid, MULTICODEC_RAW};
use mistlib_core::storage::protocol::{
    build_have_chunk_message, build_have_message, build_have_status_message, build_query_message,
    build_want_message,
};
use mistlib_core::types::NodeId;
use storage_frame::classify_raw_message;

#[test]
fn storage_raw_events_are_consumed_and_application_events_are_forwarded() {
    let cid = compute_cid(b"payload", MULTICODEC_RAW);
    for raw in [
        build_want_message(&cid),
        build_query_message(&cid),
        build_have_status_message(&cid),
        build_have_message(&cid, b"payload"),
        build_have_chunk_message(&cid, 0, 2, b"payload"),
    ] {
        let event = EngineEvent::RawMessage(NodeId("peer".into()), raw.into());
        assert!(classify_raw_message(&event).is_some());
    }
    for raw in [b"\x01hello".as_slice(), b"\x03", b"", b"\xff\x00"] {
        let event = EngineEvent::RawMessage(NodeId("peer".into()), raw.to_vec().into());
        assert!(classify_raw_message(&event).is_none());
    }
    // Only raw events participate in storage classification.
    let event = EngineEvent::OverlayMessage(NodeId("peer".into()), build_want_message(&cid));
    assert!(classify_raw_message(&event).is_none());
}
