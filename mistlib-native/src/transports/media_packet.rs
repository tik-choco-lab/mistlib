//! RTP media carried outside WebRTC (SPEC-25 KR 2.5).
//!
//! Peers carried by the ICE+QUIC link (`webrtc.iceQuic`, on by default) have no
//! WebRTC media tracks. Their media travels as marshaled RTP packets in QUIC
//! DATAGRAMs: the sender writes through [`crate::write_local_rtp`] (which also
//! writes to the WebRTC track, so WebRTC peers keep receiving it), and the
//! receiver gets one [`MediaPacketEvent`] per packet through
//! [`crate::register_media_packet_handler`]. WebRTC peers keep delivering
//! [`crate::MediaTrackEvent`] as before.

use std::sync::Mutex;

use mistlib_core::types::NodeId;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// One RTP packet received from a peer over a non-WebRTC link.
#[derive(Clone, Debug)]
pub struct MediaPacketEvent {
    pub room_id: String,
    pub remote_id: NodeId,
    /// The publisher's local track id (`TrackLocalStaticRTP::id`).
    pub track_id: String,
    pub stream_id: String,
    /// `"audio"` or `"video"`.
    pub kind: String,
    /// Codec MIME type, e.g. `"audio/opus"`.
    pub mime_type: String,
    pub clock_rate: u32,
    pub channels: u16,
    pub packet: webrtc::rtp::packet::Packet,
}

pub(crate) static GLOBAL_MEDIA_PACKET_TX: Mutex<Option<mpsc::UnboundedSender<MediaPacketEvent>>> =
    Mutex::new(None);

pub(crate) fn dispatch(event: MediaPacketEvent) {
    let tx = GLOBAL_MEDIA_PACKET_TX
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

/// What a sender announces once per track and peer before its datagrams;
/// the receiver needs it to label packets of the announced track number.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrackDescriptor {
    /// Per-peer track number carried in each media datagram.
    #[serde(default)]
    pub no: u8,
    pub id: String,
    pub stream_id: String,
    pub kind: String,
    pub mime_type: String,
    pub clock_rate: u32,
    pub channels: u16,
}
