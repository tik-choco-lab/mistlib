//! PLI/NACK RTCP feedback helpers, ported from mistlink/internal/receiver's
//! SendPLI/SendNACK (relocated here — see the deviation note in `stream/mod.rs`).

use std::sync::Arc;

use rtcp::packet::Packet;
use rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtcp::transport_feedbacks::transport_layer_nack::{NackPair, TransportLayerNack};
use webrtc::peer_connection::RTCPeerConnection;

/// Requests a keyframe from the publisher of `media_ssrc` on `pc`.
pub async fn send_pli(pc: &Arc<RTCPeerConnection>, media_ssrc: u32) {
    let pli: Box<dyn Packet + Send + Sync> = Box::new(PictureLossIndication {
        sender_ssrc: 0,
        media_ssrc,
    });
    if let Err(err) = pc.write_rtcp(&[pli]).await {
        tracing::warn!("send_pli failed for ssrc {media_ssrc}: {err}");
    }
}

/// Requests retransmission of `missing` sequence numbers for `media_ssrc` on `pc`.
pub async fn send_nack(pc: &Arc<RTCPeerConnection>, media_ssrc: u32, missing: &[u16]) {
    if missing.is_empty() {
        return;
    }

    let mut pairs = Vec::new();
    let mut iter = missing.iter().copied();
    while let Some(base) = iter.next() {
        // Pack subsequent contiguous-ish sequence numbers into the same
        // NackPair's 16-bit "following lost packets" bitmask, matching the
        // wire format's intent (mirrors what the Go pion helper produced).
        let mut bitmask: u16 = 0;
        let mut peekable = iter.clone();
        let mut consumed = 0;
        for seq in peekable.by_ref() {
            let offset = seq.wrapping_sub(base);
            if offset == 0 || offset > 16 {
                break;
            }
            bitmask |= 1 << (offset - 1);
            consumed += 1;
        }
        for _ in 0..consumed {
            iter.next();
        }
        pairs.push(NackPair {
            packet_id: base,
            lost_packets: bitmask,
        });
    }

    let nack: Box<dyn Packet + Send + Sync> = Box::new(TransportLayerNack {
        sender_ssrc: 0,
        media_ssrc,
        nacks: pairs,
    });
    if let Err(err) = pc.write_rtcp(&[nack]).await {
        tracing::warn!("send_nack failed for ssrc {media_ssrc}: {err}");
    }
}
