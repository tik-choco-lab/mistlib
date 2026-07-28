//! Media pipeline: WebRTC↔RTP handling, RTSP-free bridging, and WHIP/UDP
//! ingest — ported from mistlink (Go/pion). Builds on
//! [`crate::transports::webrtc`]'s media-track support (`Peer::add_local_track`,
//! `WebRtcTransport::set_media_track_handler`) but stays decoupled from the
//! signaling/connection-lifecycle machinery in that module: everything here
//! operates on `webrtc` crate types directly (`TrackRemote`, `RTCPeerConnection`,
//! RTP packets) so it has no dependency on `NodeId`/`Signaler`/`Peer`.
//!
//! This was originally a separate `mistlib-media` crate; folded in here
//! because mistlib is a general-purpose networking library and media
//! handling is a native-transport concern, not something that needs its own
//! crate boundary.
//!
//! The HLS/MPEG-TS test-output stack (MPEG-TS muxing, HLS segmenting, the
//! plain HTTP server for `.m3u8`/`.ts`) is deliberately *not* part of this
//! library API — it's local playback-testing tooling (e.g. for VRChat's
//! AVPro Video player), not something a general networking library should
//! ship. It lives as shared support code under
//! `examples/hls_support/` instead, pulled in via `#[path]` by
//! `examples/hls_preview.rs`, `examples/hls_preview_live.rs`, and
//! `tests/loopback_media.rs`.

pub mod error;
pub mod input;
pub mod receiver;
pub mod rtp;
pub mod stream;

pub use error::{MediaError, Result};
