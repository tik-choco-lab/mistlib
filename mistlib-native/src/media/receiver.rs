//! RTP receiving/bridging, ported from mistlink/internal/receiver (Go/pion).
//!
//! Scope boundary: the Go original embedded a full RTSP server
//! (`internal/rtsp`, gortsplib) inside `RTPBridge` and wrote paced/rewritten
//! RTP packets directly to it. That RTSP server is app-side tooling, not a
//! media-pipeline concern, so this port does **not** embed one. Instead
//! [`RtpBridge`] exposes the same information as a plain async API:
//! - [`RtpBridge::subscribe`] — a broadcast stream of paced, reordered RTP
//!   packets (video/audio interleaved, SSRC-rewritten per `rtp::nal`), which
//!   an app-side RTSP server (or anything else) can consume directly.
//! - [`RtpBridge::sps_pps`] — the cached SPS/PPS once both are known, so a
//!   consumer can build a session description before packets start flowing.
//! - [`RtpBridge::request_keyframe`]/[`RtpBridge::subscribe_keyframe_requests`]
//!   — replaces the Go original's `RegisterPLIHandler`/`RequestIDR` callback
//!   registry with a broadcast channel of the same intent.
//!
//! Deviation: the Go `RTSPBuffer` tuned its flush interval adaptively based
//! on measured packets-per-second (10–100ms). This port paces on a fixed
//! interval (`PACE_INTERVAL`) instead — the adaptive tuning was purely an
//! RTSP-pacing optimization with no bearing on the API boundary above, and
//! cutting it keeps this port bounded. Revisit if real playback shows pacing
//! issues under variable bitrate.

mod bridge;
mod buffer;
mod stats;

pub use bridge::RtpBridge;
pub use buffer::OutputBuffer;
pub use stats::TrackStats;
