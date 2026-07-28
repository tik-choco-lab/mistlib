//! Trait bridging `TrackBroadcaster` to an RTSP/loopback consumer, decoupling
//! `stream` from the not-yet-ported `receiver` module (see `mod.rs` deviations).

/// Mirrors the subset of `internal/receiver.RTPBridge`'s API that
/// `TrackBroadcaster` (ported from `internal/stream/broadcaster.go`) calls.
pub trait RtpSink: Send + Sync {
    /// Called once when a broadcaster starts reading a track. Returns an
    /// opaque track id to pass to subsequent `write_rtp`/`track_stopped` calls.
    fn track_started(&self, ssrc: u32, mime_type: &str) -> String;

    /// Called once when a broadcaster stops reading a track.
    fn track_stopped(&self, ssrc: u32, track_id: &str);

    /// Whether the sink is actively consuming (used to decide whether to
    /// proactively request keyframes via PLI).
    fn is_started(&self) -> bool;

    /// Forwards a single depacketized RTP packet for `track_id` to the sink.
    fn write_rtp(&self, packet: &rtp::packet::Packet, track_id: &str);
}
