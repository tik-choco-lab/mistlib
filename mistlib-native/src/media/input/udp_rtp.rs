//! Ingests already-RTP-packetized UDP datagrams into an [`RtpBridge`].
//! Ported from mistlink/internal/sender/mpegts_to_rtp.go's
//! `HandleMPEGTSStream` — despite the Go filename, that function never
//! demuxed MPEG-TS itself; it read UDP datagrams that some external process
//! (e.g. `ffmpeg -f rtp`) had *already* RTP-packetized from MPEG-TS, and
//! just relayed them. Renamed here (`udp_rtp`, not `mpegts_to_rtp`) to avoid
//! perpetuating that misnomer.
//!
//! Not part of `stream::TrackBroadcaster`'s WebRTC-track path — this is for
//! a publisher that pushes RTP directly over UDP instead of via a WebRTC
//! `on_track` (mistlink used this as the fallback input when no OBS/WHIP
//! track was present, see `sender/pc_factory.go`'s `EnsureOutgoingTracks`).

use std::collections::HashMap;
use std::sync::Arc;

use util::Unmarshal;

use crate::media::receiver::RtpBridge;
use crate::media::rtp::nal;
use crate::media::stream::RtpSink;

/// Parses one UDP datagram as an RTP packet and, if its payload type is
/// H264 or Opus, routes it into `bridge`, registering a new SSRC with the
/// bridge (via `RtpSink::track_started`) the first time it's seen. Returns
/// the `rtp` crate's unmarshal error for malformed datagrams; the caller
/// should log and continue rather than treat this as fatal (as the Go
/// original did with "RTP parse error... skip").
pub fn route_datagram(
    buf: &[u8],
    bridge: &RtpBridge,
    track_ids: &mut HashMap<u32, String>,
) -> Result<(), rtp::Error> {
    let mut cursor = bytes::Bytes::copy_from_slice(buf);
    let pkt = rtp::packet::Packet::unmarshal(&mut cursor)?;
    if pkt.payload.is_empty() {
        return Ok(());
    }

    let mime = match pkt.header.payload_type {
        nal::PAYLOAD_TYPE_H264 => "video/h264",
        nal::PAYLOAD_TYPE_OPUS => "audio/opus",
        _ => return Ok(()),
    };

    let ssrc = pkt.header.ssrc;
    let track_id = track_ids
        .entry(ssrc)
        .or_insert_with(|| bridge.track_started(ssrc, mime))
        .clone();

    bridge.write_rtp(&pkt, &track_id);
    Ok(())
}

/// Runs the UDP receive loop until the socket errors, routing every
/// datagram via [`route_datagram`]. Spawn as a background task.
pub async fn run(socket: tokio::net::UdpSocket, bridge: Arc<RtpBridge>) -> std::io::Result<()> {
    let mut buf = vec![0u8; 1500];
    let mut track_ids = HashMap::new();
    loop {
        let n = socket.recv(&mut buf).await?;
        if let Err(err) = route_datagram(&buf[..n], &bridge, &mut track_ids) {
            tracing::warn!("udp rtp parse error: {err} (skip)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use util::Marshal;

    fn encode_packet(payload_type: u8, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let pkt = rtp::packet::Packet {
            header: rtp::header::Header {
                payload_type,
                ssrc,
                ..Default::default()
            },
            payload: bytes::Bytes::copy_from_slice(payload),
        };
        pkt.marshal().unwrap().to_vec()
    }

    #[tokio::test]
    async fn h264_datagram_registers_track_and_writes_rtp() {
        let bridge = RtpBridge::new(0);
        let mut track_ids = HashMap::new();
        let datagram = encode_packet(nal::PAYLOAD_TYPE_H264, 42, &[0x67, 0xAA]); // SPS

        route_datagram(&datagram, &bridge, &mut track_ids).unwrap();
        assert_eq!(track_ids.len(), 1);
        assert!(track_ids.contains_key(&42));

        let sps_datagram = encode_packet(nal::PAYLOAD_TYPE_H264, 42, &[0x68, 0xBB]); // PPS
        route_datagram(&sps_datagram, &bridge, &mut track_ids).unwrap();
        assert!(bridge.is_started());
        bridge.stop();
    }

    #[tokio::test]
    async fn unrelated_payload_type_is_ignored_without_error() {
        let bridge = RtpBridge::new(0);
        let mut track_ids = HashMap::new();
        let datagram = encode_packet(99, 1, &[0x01]);
        route_datagram(&datagram, &bridge, &mut track_ids).unwrap();
        assert!(track_ids.is_empty());
        bridge.stop();
    }

    #[tokio::test]
    async fn malformed_datagram_returns_error() {
        let bridge = RtpBridge::new(0);
        let mut track_ids = HashMap::new();
        let result = route_datagram(&[0x00, 0x01], &bridge, &mut track_ids);
        assert!(result.is_err());
        bridge.stop();
    }
}
