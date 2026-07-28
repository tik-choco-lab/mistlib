//! External (non-WebRTC) RTP ingest paths for a publisher — currently just
//! UDP. Counterpart to `output/`: where `output/` turns a bridge's stream
//! into HLS for external playback, `input/` turns an external RTP source
//! into a bridge's stream.

pub mod udp_rtp;
pub mod whip;

pub use udp_rtp::{route_datagram, run};
pub use whip::{serve as serve_whip, OnTrack as WhipOnTrack};
