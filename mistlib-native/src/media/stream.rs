//! WebRTC media-track forwarding, ported from mistlink/internal/stream (Go/pion).
//!
//! Deviations from the Go original, and why:
//! - The Go `TrackBroadcaster` depended directly on `internal/receiver.RTPBridge`.
//!   Since the receiver module hasn't been ported yet, this crate defines the
//!   [`RtpSink`] trait instead so `stream` doesn't need to know about RTSP/bridge
//!   internals; the future receiver port implements it.
//! - SPS/PPS caching: the Go code cached raw NAL bytes and passed them straight
//!   into `TrackLocalStaticRTP.Write`, which expects a *marshaled RTP packet*,
//!   not a bare NAL unit — likely a latent bug. This port caches the actual
//!   `rtp::packet::Packet` that carried the SPS/PPS and replays that packet
//!   (via `write_rtp_with_extensions`), which is well-typed in webrtc-rs and
//!   avoids the mismatch entirely.
//! - PLI/NACK sending is exposed by this module as [`send_pli`]/[`send_nack`]
//!   (writing RTCP back to the publisher's `RTCPeerConnection`), since the Go
//!   versions lived in `internal/receiver` but are really stream/broadcast
//!   concerns; `receiver`'s future port can reuse or re-export these.

mod broadcaster;
mod manager;
mod relay;
mod rtcp;
mod sink;

pub use broadcaster::TrackBroadcaster;
pub use manager::StreamManager;
pub use relay::relay_to_local_tracks;
pub use rtcp::{send_nack, send_pli};
pub use sink::RtpSink;
