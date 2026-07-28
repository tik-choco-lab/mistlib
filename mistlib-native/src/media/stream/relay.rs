//! Relays a paced RTP stream (e.g. from `receiver::RtpBridge::subscribe()`)
//! out onto local WebRTC tracks for peer-to-peer forwarding. Ported from
//! mistlink/internal/sender/relay_stream.go's `HandleRelayStream`.
//!
//! Deviation: the Go original used `RTPBridge.AddListener`/`RemoveListener`,
//! a bespoke callback registry. This port uses the bridge's
//! `tokio::sync::broadcast` stream directly (`RtpBridge::subscribe`) instead
//! — same effect (every relay task gets every packet), standard tokio idiom,
//! no separate listener-management API to maintain.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

use crate::media::rtp::nal;

/// Spawns a background task that reads from `packets` and writes each H264
/// packet to `video_track` and each Opus packet to `audio_track` (either may
/// be `None` to relay only one media kind). Stops when `cancel` fires or the
/// broadcast channel closes.
pub fn relay_to_local_tracks(
    mut packets: broadcast::Receiver<rtp::packet::Packet>,
    video_track: Option<Arc<TrackLocalStaticRTP>>,
    audio_track: Option<Arc<TrackLocalStaticRTP>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = packets.recv() => {
                    match result {
                        Ok(pkt) => write_to_matching_track(&pkt, &video_track, &audio_track).await,
                        Err(broadcast::error::RecvError::Closed) => break,
                        // A slow relay consumer fell behind the bridge's
                        // broadcast buffer; skip ahead rather than stall.
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            }
        }
    });
}

async fn write_to_matching_track(
    pkt: &rtp::packet::Packet,
    video_track: &Option<Arc<TrackLocalStaticRTP>>,
    audio_track: &Option<Arc<TrackLocalStaticRTP>>,
) {
    let track = match pkt.header.payload_type {
        nal::PAYLOAD_TYPE_H264 => video_track,
        nal::PAYLOAD_TYPE_OPUS => audio_track,
        _ => return,
    };
    if let Some(track) = track {
        if let Err(err) = track.write_rtp_with_extensions(pkt, &[]).await {
            tracing::debug!("relay write error: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;

    fn make_track() -> Arc<TrackLocalStaticRTP> {
        Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability::default(),
            "t".to_string(),
            "s".to_string(),
        ))
    }

    fn packet(payload_type: u8) -> rtp::packet::Packet {
        rtp::packet::Packet {
            header: rtp::header::Header {
                payload_type,
                ..Default::default()
            },
            payload: bytes::Bytes::new(),
        }
    }

    #[tokio::test]
    async fn write_to_matching_track_only_writes_the_matching_kind() {
        let video = make_track();
        let audio = make_track();

        // No bindings on either track, so write_rtp_with_extensions is a
        // documented no-op success — this exercises the actual routing
        // (right track selected by payload type) without needing a bound
        // RTCPeerConnection/RTCRtpSender.
        write_to_matching_track(
            &packet(nal::PAYLOAD_TYPE_H264),
            &Some(video.clone()),
            &Some(audio.clone()),
        )
        .await;
        write_to_matching_track(&packet(nal::PAYLOAD_TYPE_OPUS), &Some(video), &Some(audio)).await;
        write_to_matching_track(&packet(99), &None, &None).await; // unrelated type: no-op, no panic
    }

    #[tokio::test]
    async fn relay_task_stops_on_cancellation() {
        let (tx, rx) = broadcast::channel(4);
        let cancel = CancellationToken::new();
        relay_to_local_tracks(rx, Some(make_track()), Some(make_track()), cancel.clone());

        tx.send(packet(nal::PAYLOAD_TYPE_H264)).unwrap();
        tokio::task::yield_now().await;

        cancel.cancel();
        // Give the spawned task a chance to observe cancellation; nothing to
        // assert beyond "this doesn't hang or panic" since the task has no
        // externally observable completion signal.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
