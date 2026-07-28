//! Ported from mistlink/internal/receiver/{bridge,rtp_sender,sps_pps}.go —
//! see the API-boundary note in `receiver/mod.rs` for what's intentionally
//! different from the Go original (no embedded RTSP server).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::media::receiver::buffer::OutputBuffer;
use crate::media::rtp::{nal, sps_pps::extract_sps_pps};
use crate::media::stream::RtpSink;

const PACE_INTERVAL: Duration = Duration::from_millis(50);
const PACKET_CHANNEL_CAPACITY: usize = 1024;
const KEYFRAME_CHANNEL_CAPACITY: usize = 16;

#[derive(Default)]
struct Inner {
    active_tracks: HashMap<u32, String>,
    primary_video_ssrc: Option<u32>,
    primary_audio_ssrc: Option<u32>,
    active_video_track_id: Option<String>,
    active_audio_track_id: Option<String>,
}

/// Bridges WebRTC RTP tracks (via [`RtpSink`]) into a paced, reordered
/// packet stream plus SPS/PPS availability, for an app-side consumer (e.g.
/// an RTSP server) to subscribe to. See `receiver/mod.rs` for the full
/// boundary rationale.
pub struct RtpBridge {
    inner: Mutex<Inner>,
    sps: RwLock<Option<Bytes>>,
    pps: RwLock<Option<Bytes>>,
    started: AtomicBool,
    buffer: Mutex<OutputBuffer>,
    packet_tx: broadcast::Sender<rtp::packet::Packet>,
    keyframe_tx: broadcast::Sender<u32>,
    cancel: CancellationToken,
}

impl RtpBridge {
    /// `buffer_size` mirrors the Go `RTPBridge`'s constructor parameter
    /// (0 = default). Spawns the background pacing task immediately.
    pub fn new(buffer_size: usize) -> Arc<Self> {
        let (packet_tx, _) = broadcast::channel(PACKET_CHANNEL_CAPACITY);
        let (keyframe_tx, _) = broadcast::channel(KEYFRAME_CHANNEL_CAPACITY);
        let this = Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            sps: RwLock::new(None),
            pps: RwLock::new(None),
            started: AtomicBool::new(false),
            buffer: Mutex::new(OutputBuffer::new(buffer_size)),
            packet_tx,
            keyframe_tx,
            cancel: CancellationToken::new(),
        });
        this.clone().spawn_pacer();
        this
    }

    fn spawn_pacer(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PACE_INTERVAL);
            loop {
                tokio::select! {
                    _ = self.cancel.cancelled() => break,
                    _ = interval.tick() => {
                        let packets = self.buffer.lock().unwrap().flush_ready();
                        for pkt in packets {
                            // No subscribers is a normal, common state; ignore.
                            let _ = self.packet_tx.send(pkt);
                        }
                    }
                }
            }
        });
    }

    /// Stops the pacing task. The bridge is unusable afterwards.
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Subscribes to the paced, reordered, SSRC-rewritten output stream
    /// (video and audio interleaved, distinguishable by `payload_type`).
    pub fn subscribe(&self) -> broadcast::Receiver<rtp::packet::Packet> {
        self.packet_tx.subscribe()
    }

    /// Subscribes to keyframe (PLI) requests, identified by SSRC. Replaces
    /// the Go original's `RegisterPLIHandler`/`RequestIDR` callback registry;
    /// the consumer is expected to translate these into RTCP PLI packets
    /// (e.g. via `stream::send_pli`).
    pub fn subscribe_keyframe_requests(&self) -> broadcast::Receiver<u32> {
        self.keyframe_tx.subscribe()
    }

    /// Returns the cached SPS/PPS once both have been seen.
    pub fn sps_pps(&self) -> Option<(Bytes, Bytes)> {
        let sps = self.sps.read().unwrap().clone()?;
        let pps = self.pps.read().unwrap().clone()?;
        Some((sps, pps))
    }

    /// Directly sets SPS/PPS (e.g. from `rtp::sdp::extract_sps_pps_from_sdp`),
    /// matching the Go original's `SetSPSPPS`: only fills in values not
    /// already cached.
    pub fn set_sps_pps(&self, sps: Option<Bytes>, pps: Option<Bytes>) {
        let mut changed = false;
        if let Some(sps) = sps {
            let mut slot = self.sps.write().unwrap();
            if slot.is_none() {
                *slot = Some(sps);
                changed = true;
            }
        }
        if let Some(pps) = pps {
            let mut slot = self.pps.write().unwrap();
            if slot.is_none() {
                *slot = Some(pps);
                changed = true;
            }
        }
        if changed {
            self.try_start();
        }
    }

    fn try_start(&self) {
        if self.started.load(Ordering::SeqCst) {
            return;
        }
        let has_both = self.sps.read().unwrap().is_some() && self.pps.read().unwrap().is_some();
        if has_both {
            self.started.store(true, Ordering::SeqCst);
        }
    }

    fn extract_sps_pps_from_payload(&self, payload: &[u8]) {
        let nal_type = nal::get_nal_type(payload);
        let Some(found) = extract_sps_pps(payload, nal_type) else {
            return;
        };
        self.set_sps_pps(found.sps, found.pps);
    }
}

impl RtpSink for RtpBridge {
    fn track_started(&self, ssrc: u32, mime_type: &str) -> String {
        let mime = mime_type.to_lowercase();
        let is_audio = mime == "audio/opus" || mime == "audio/aac";
        let is_video =
            mime.starts_with("video/") && (mime.contains("h264") || mime.contains("avc"));

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let track_id = format!("{ssrc}_{nanos}");

        {
            let mut inner = self.inner.lock().unwrap();
            inner.active_tracks.insert(ssrc, mime.clone());

            if is_video {
                match inner.primary_video_ssrc {
                    None => {
                        inner.primary_video_ssrc = Some(ssrc);
                        inner.active_video_track_id = Some(track_id.clone());
                    }
                    Some(primary) if primary != ssrc => {
                        tracing::warn!(
                            "ignoring additional video track ssrc={ssrc} (primary={primary})"
                        );
                    }
                    Some(_) => {
                        inner.active_video_track_id = Some(track_id.clone());
                    }
                }
            } else if is_audio {
                match inner.primary_audio_ssrc {
                    None => {
                        inner.primary_audio_ssrc = Some(ssrc);
                        inner.active_audio_track_id = Some(track_id.clone());
                    }
                    Some(primary) if primary != ssrc => {
                        tracing::warn!(
                            "ignoring additional audio track ssrc={ssrc} (primary={primary})"
                        );
                    }
                    Some(_) => {
                        inner.active_audio_track_id = Some(track_id.clone());
                    }
                }
            }
        }

        if is_video {
            let _ = self.keyframe_tx.send(ssrc);
        }

        track_id
    }

    fn track_stopped(&self, ssrc: u32, track_id: &str) {
        let no_active_tracks = {
            let mut inner = self.inner.lock().unwrap();
            inner.active_tracks.remove(&ssrc);

            if inner.primary_video_ssrc == Some(ssrc)
                && inner.active_video_track_id.as_deref() == Some(track_id)
            {
                inner.primary_video_ssrc = None;
                inner.active_video_track_id = None;
            }
            if inner.primary_audio_ssrc == Some(ssrc)
                && inner.active_audio_track_id.as_deref() == Some(track_id)
            {
                inner.primary_audio_ssrc = None;
                inner.active_audio_track_id = None;
            }

            inner.active_tracks.is_empty()
        };

        if no_active_tracks {
            self.started.store(false, Ordering::SeqCst);
            *self.sps.write().unwrap() = None;
            *self.pps.write().unwrap() = None;
            self.buffer.lock().unwrap().clear();
        }
    }

    fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    fn write_rtp(&self, packet: &rtp::packet::Packet, track_id: &str) {
        let (allowed, is_video_track) = {
            let inner = self.inner.lock().unwrap();
            let is_video = inner.active_video_track_id.as_deref() == Some(track_id);
            let is_audio = inner.active_audio_track_id.as_deref() == Some(track_id);
            (is_video || is_audio, is_video)
        };
        if !allowed {
            return;
        }

        if is_video_track {
            self.extract_sps_pps_from_payload(&packet.payload);
        }

        self.buffer.lock().unwrap().add(packet.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_pkt(seq: u16, payload: Vec<u8>) -> rtp::packet::Packet {
        rtp::packet::Packet {
            header: rtp::header::Header {
                payload_type: nal::PAYLOAD_TYPE_H264,
                sequence_number: seq,
                ..Default::default()
            },
            payload: payload.into(),
        }
    }

    #[tokio::test]
    async fn track_started_assigns_primary_video_and_requests_keyframe() {
        let bridge = RtpBridge::new(0);
        let mut keyframes = bridge.subscribe_keyframe_requests();

        let track_id = bridge.track_started(42, "video/H264");
        assert!(track_id.starts_with("42_"));
        assert_eq!(keyframes.try_recv().unwrap(), 42);
        bridge.stop();
    }

    #[tokio::test]
    async fn write_rtp_from_non_active_track_is_ignored() {
        let bridge = RtpBridge::new(0);
        // No track_started call, so "unknown" is not an active track id.
        bridge.write_rtp(&video_pkt(1, vec![0x67, 0x01]), "unknown");
        assert!(bridge.sps_pps().is_none());
        bridge.stop();
    }

    #[tokio::test]
    async fn sps_and_pps_from_active_video_track_start_the_bridge() {
        let bridge = RtpBridge::new(0);
        let track_id = bridge.track_started(7, "video/H264");

        assert!(!bridge.is_started());
        bridge.write_rtp(&video_pkt(1, vec![0x67, 0xAA]), &track_id); // SPS
        assert!(!bridge.is_started());
        bridge.write_rtp(&video_pkt(2, vec![0x68, 0xBB]), &track_id); // PPS
        assert!(bridge.is_started());

        let (sps, pps) = bridge.sps_pps().unwrap();
        assert_eq!(&sps[..], &[0x67, 0xAA]);
        assert_eq!(&pps[..], &[0x68, 0xBB]);
        bridge.stop();
    }

    #[tokio::test]
    async fn track_stopped_clears_state_once_no_tracks_remain() {
        let bridge = RtpBridge::new(0);
        let track_id = bridge.track_started(7, "video/H264");
        bridge.write_rtp(&video_pkt(1, vec![0x67, 0xAA]), &track_id);
        bridge.write_rtp(&video_pkt(2, vec![0x68, 0xBB]), &track_id);
        assert!(bridge.is_started());

        bridge.track_stopped(7, &track_id);
        assert!(!bridge.is_started());
        assert!(bridge.sps_pps().is_none());
        bridge.stop();
    }

    #[tokio::test]
    async fn set_sps_pps_does_not_overwrite_existing_values() {
        let bridge = RtpBridge::new(0);
        bridge.set_sps_pps(
            Some(Bytes::from_static(b"sps1")),
            Some(Bytes::from_static(b"pps1")),
        );
        bridge.set_sps_pps(Some(Bytes::from_static(b"sps2")), None);

        let (sps, pps) = bridge.sps_pps().unwrap();
        assert_eq!(&sps[..], b"sps1");
        assert_eq!(&pps[..], b"pps1");
        bridge.stop();
    }
}
