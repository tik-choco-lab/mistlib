//! Ported from mistlink/internal/stream/broadcaster.go.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::RwLock as AsyncRwLock;
use tokio_util::sync::CancellationToken;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_remote::TrackRemote;

use crate::media::rtp::nal;
use crate::media::rtp::sps_pps::extract_sps_pps;
use crate::media::stream::rtcp::{send_nack, send_pli};
use crate::media::stream::sink::RtpSink;

const H264_MIME_TYPE: &str = "video/h264";
const OPUS_MIME_TYPE: &str = "audio/opus";
const PLI_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Reads RTP packets off one remote track and fans them out to subscribed
/// local tracks (viewers) and, optionally, an [`RtpSink`] (e.g. RTSP loopback).
pub struct TrackBroadcaster {
    track: Arc<TrackRemote>,
    pc: Arc<RTCPeerConnection>,
    sink: Option<Arc<dyn RtpSink>>,

    receivers: AsyncRwLock<HashMap<String, Arc<TrackLocalStaticRTP>>>,

    // Cached keyframe parameter sets, kept as full RTP packets (not bare NAL
    // bytes — see the deviation note in `stream/mod.rs`) so they can be
    // replayed verbatim to newly-joined receivers.
    sps: RwLock<Option<rtp::packet::Packet>>,
    pps: RwLock<Option<rtp::packet::Packet>>,

    cancel: CancellationToken,
}

impl TrackBroadcaster {
    pub fn new(
        track: Arc<TrackRemote>,
        pc: Arc<RTCPeerConnection>,
        sink: Option<Arc<dyn RtpSink>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            track,
            pc,
            sink,
            receivers: AsyncRwLock::new(HashMap::new()),
            sps: RwLock::new(None),
            pps: RwLock::new(None),
            cancel: CancellationToken::new(),
        })
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Registers a new viewer's local track and immediately replays any cached
    /// SPS/PPS so the viewer can start decoding without waiting for the next
    /// keyframe interval; also requests a fresh keyframe from the publisher.
    pub async fn add_receiver(self: &Arc<Self>, id: String, local_track: Arc<TrackLocalStaticRTP>) {
        {
            let mut receivers = self.receivers.write().await;
            receivers.insert(id.clone(), local_track.clone());
        }

        let cached_sps = self.sps.read().unwrap().clone();
        if let Some(sps) = cached_sps {
            if let Err(err) = local_track.write_rtp_with_extensions(&sps, &[]).await {
                tracing::error!("error sending cached SPS to {id}: {err}");
            }
        }
        let cached_pps = self.pps.read().unwrap().clone();
        if let Some(pps) = cached_pps {
            if let Err(err) = local_track.write_rtp_with_extensions(&pps, &[]).await {
                tracing::error!("error sending cached PPS to {id}: {err}");
            }
        }

        let pc = self.pc.clone();
        let ssrc = self.track.ssrc();
        tokio::spawn(async move {
            send_pli(&pc, ssrc).await;
        });
    }

    pub async fn remove_receiver(&self, id: &str) {
        self.receivers.write().await.remove(id);
    }

    /// Spawns the read loop as a background task. Call [`TrackBroadcaster::stop`]
    /// to terminate it.
    pub fn start(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            this.run().await;
        });
    }

    async fn run(self: Arc<Self>) {
        let mime_type = self.track.codec().capability.mime_type.to_lowercase();
        let is_video = mime_type == H264_MIME_TYPE;
        let is_audio = mime_type == OPUS_MIME_TYPE;
        if !is_video && !is_audio {
            return;
        }

        let ssrc = self.track.ssrc();
        let track_id = self
            .sink
            .as_ref()
            .map(|s| s.track_started(ssrc, &mime_type));

        tracing::debug!("broadcaster started for track: {mime_type} (ssrc={ssrc})");

        let mut stats = TrackStats::default();
        let mut last_pli = Instant::now();

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                result = self.track.read_rtp() => {
                    let (mut pkt, _) = match result {
                        Ok(v) => v,
                        Err(err) => {
                            tracing::error!("broadcaster read error: {err}");
                            break;
                        }
                    };

                    if is_video {
                        let missing = stats.check_sequence_gap(pkt.header.sequence_number);
                        if !missing.is_empty() {
                            send_nack(&self.pc, ssrc, &missing).await;
                        }

                        let sink_started = self.sink.as_ref().is_some_and(|s| s.is_started());
                        if !sink_started && last_pli.elapsed() > PLI_RETRY_INTERVAL {
                            send_pli(&self.pc, ssrc).await;
                            last_pli = Instant::now();
                        }

                        self.cache_sps_pps(&pkt);

                        pkt.header.payload_type = nal::PAYLOAD_TYPE_H264;
                    } else if is_audio {
                        pkt.header.payload_type = nal::PAYLOAD_TYPE_OPUS;
                    }

                    // Forwarded for both video and audio, matching the Go
                    // original's RTPBridge.WriteRTP (which is gated on
                    // activeVideoTrackID *or* activeAudioTrackID, not video-only).
                    if let (Some(sink), Some(track_id)) = (&self.sink, &track_id) {
                        sink.write_rtp(&pkt, track_id);
                    }

                    let receivers = self.receivers.read().await;
                    for (id, local_track) in receivers.iter() {
                        if let Err(err) = local_track.write_rtp_with_extensions(&pkt, &[]).await {
                            tracing::error!("error writing to receiver {id}: {err}");
                        }
                    }

                    stats.packet_count += 1;
                }
            }
        }

        if let (Some(sink), Some(track_id)) = (&self.sink, &track_id) {
            sink.track_stopped(ssrc, track_id);
        }
    }

    fn cache_sps_pps(&self, pkt: &rtp::packet::Packet) {
        let nal_type = nal::get_nal_type(&pkt.payload);
        let Some(found) = extract_sps_pps(&pkt.payload, nal_type) else {
            return;
        };
        if found.sps.is_some() {
            *self.sps.write().unwrap() = Some(pkt.clone());
        }
        if found.pps.is_some() {
            *self.pps.write().unwrap() = Some(pkt.clone());
        }
    }
}

#[derive(Default)]
struct TrackStats {
    last_seq: Option<u16>,
    packet_count: u64,
}

impl TrackStats {
    /// Returns the sequence numbers skipped since the last packet, matching
    /// the Go original's gap window (ignores gaps >100, treating them as a
    /// stream discontinuity rather than genuine loss).
    fn check_sequence_gap(&mut self, seq: u16) -> Vec<u16> {
        let Some(last_seq) = self.last_seq else {
            self.last_seq = Some(seq);
            return Vec::new();
        };

        let diff = seq.wrapping_sub(last_seq);
        self.last_seq = Some(seq);
        if diff <= 1 || diff > 100 {
            return Vec::new();
        }

        (1..diff).map(|i| last_seq.wrapping_add(i)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_sequence_gap_no_gap_returns_empty() {
        let mut stats = TrackStats::default();
        assert!(stats.check_sequence_gap(1).is_empty());
        assert!(stats.check_sequence_gap(2).is_empty());
    }

    #[test]
    fn check_sequence_gap_reports_missing_sequence_numbers() {
        let mut stats = TrackStats::default();
        stats.check_sequence_gap(10);
        let missing = stats.check_sequence_gap(13);
        assert_eq!(missing, vec![11, 12]);
    }

    #[test]
    fn check_sequence_gap_ignores_large_jumps() {
        let mut stats = TrackStats::default();
        stats.check_sequence_gap(10);
        assert!(stats.check_sequence_gap(200).is_empty());
    }

    #[test]
    fn check_sequence_gap_handles_u16_wraparound() {
        let mut stats = TrackStats::default();
        stats.check_sequence_gap(65534);
        let missing = stats.check_sequence_gap(1);
        assert_eq!(missing, vec![65535, 0]);
    }
}
