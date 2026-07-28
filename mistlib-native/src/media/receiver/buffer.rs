//! Ported from mistlink/internal/receiver/rtsp_buffer.go (`RTSPBuffer`),
//! minus the embedded RTSP writer — see the boundary note in `receiver/mod.rs`.
//!
//! Reorders and paces RTP packets per stream (video/audio), rewriting SSRC
//! and producing a continuous output timestamp/sequence, exactly like the Go
//! original's `flushBufferSet`. The difference is only in what happens with
//! the flushed packets: the Go version wrote them to an RTSP server; this
//! version just returns them from [`OutputBuffer::flush_ready`] for the
//! caller (`RtpBridge`) to broadcast.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::media::rtp::nal;

const DEFAULT_BUFFER_SIZE: usize = 2000;
const MIN_MAX_BUFFER_SIZE: usize = 1000;
const MAX_FLUSH_BATCH: usize = 500;
const MAX_STALE_PACKET_AGE: Duration = Duration::from_millis(100);

struct BufferedPacket {
    pkt: rtp::packet::Packet,
    received: Instant,
}

#[derive(Default)]
struct StreamState {
    slots: HashMap<u16, BufferedPacket>,
    order: VecDeque<u16>,
    next_seq: Option<u16>,
    outgoing_seq: Option<u16>,
    last_input_ts: Option<u32>,
    last_output_ts: Option<u32>,
}

/// Reorders and paces incoming RTP packets into two independent streams
/// (video/audio, split by payload type), ready to be flushed on a timer.
pub struct OutputBuffer {
    max_per_stream: usize,
    video: StreamState,
    audio: StreamState,
}

impl OutputBuffer {
    pub fn new(buffer_size: usize) -> Self {
        let size = if buffer_size == 0 {
            DEFAULT_BUFFER_SIZE
        } else {
            buffer_size
        };
        Self {
            max_per_stream: (size / 2).max(MIN_MAX_BUFFER_SIZE),
            video: StreamState::default(),
            audio: StreamState::default(),
        }
    }

    pub fn add(&mut self, pkt: rtp::packet::Packet) {
        let state = if pkt.header.payload_type == nal::PAYLOAD_TYPE_OPUS {
            &mut self.audio
        } else {
            &mut self.video
        };
        Self::add_to(state, pkt, self.max_per_stream);
    }

    fn add_to(state: &mut StreamState, pkt: rtp::packet::Packet, max_per_stream: usize) {
        let seq = pkt.header.sequence_number;

        if state.order.len() >= max_per_stream {
            if let Some(oldest) = state.order.pop_front() {
                state.slots.remove(&oldest);
            }
        }

        if state
            .slots
            .insert(
                seq,
                BufferedPacket {
                    pkt,
                    received: Instant::now(),
                },
            )
            .is_none()
        {
            state.order.push_back(seq);
        }
    }

    /// Drains up to `MAX_FLUSH_BATCH` in-order (or stale-timed-out) packets
    /// from each of the video/audio streams, rewriting SSRC and producing a
    /// continuous output timestamp/sequence per stream. Returns the combined,
    /// per-stream-ordered list of packets ready to send.
    pub fn flush_ready(&mut self) -> Vec<rtp::packet::Packet> {
        let mut out = Self::flush_stream(&mut self.video, nal::VIDEO_SSRC);
        out.extend(Self::flush_stream(&mut self.audio, nal::AUDIO_SSRC));
        out
    }

    fn flush_stream(state: &mut StreamState, out_ssrc: u32) -> Vec<rtp::packet::Packet> {
        let mut out = Vec::new();
        if state.order.is_empty() {
            return out;
        }

        let mut next_seq = state.next_seq.unwrap_or(state.order[0]);

        let mut sent = 0;
        while sent < MAX_FLUSH_BATCH && !state.order.is_empty() {
            let Some(buffered) = state.slots.get(&next_seq) else {
                let oldest = state.order[0];
                let is_stale = state
                    .slots
                    .get(&oldest)
                    .is_some_and(|p| p.received.elapsed() > MAX_STALE_PACKET_AGE);
                if is_stale {
                    next_seq = oldest;
                    continue;
                }
                break;
            };

            let original_ts = buffered.pkt.header.timestamp;
            let (out_ts, out_seq) = match (state.last_output_ts, state.outgoing_seq) {
                (Some(last_out_ts), Some(mut out_seq)) => {
                    let last_in_ts = state.last_input_ts.unwrap_or(original_ts);
                    let delta = original_ts.wrapping_sub(last_in_ts);
                    let delta = if delta > nal::MAX_TIMESTAMP_DELTA {
                        0
                    } else {
                        delta
                    };
                    out_seq = out_seq.wrapping_add(1);
                    (last_out_ts.wrapping_add(delta), out_seq)
                }
                _ => (
                    buffered.pkt.header.timestamp,
                    buffered.pkt.header.sequence_number,
                ),
            };

            let mut pkt = state.slots.remove(&next_seq).unwrap().pkt;
            if let Some(pos) = state.order.iter().position(|&s| s == next_seq) {
                state.order.remove(pos);
            }

            pkt.header.ssrc = out_ssrc;
            pkt.header.timestamp = out_ts;
            pkt.header.sequence_number = out_seq;

            state.last_input_ts = Some(original_ts);
            state.last_output_ts = Some(out_ts);
            state.outgoing_seq = Some(out_seq);

            out.push(pkt);

            next_seq = next_seq.wrapping_add(1);
            sent += 1;
        }

        state.next_seq = Some(next_seq);
        out
    }

    pub fn clear(&mut self) {
        self.video = StreamState::default();
        self.audio = StreamState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_pkt(seq: u16, ts: u32) -> rtp::packet::Packet {
        rtp::packet::Packet {
            header: rtp::header::Header {
                payload_type: nal::PAYLOAD_TYPE_H264,
                sequence_number: seq,
                timestamp: ts,
                ..Default::default()
            },
            payload: bytes::Bytes::new(),
        }
    }

    #[test]
    fn flush_ready_with_empty_buffer_returns_empty() {
        let mut buf = OutputBuffer::new(0);
        assert!(buf.flush_ready().is_empty());
    }

    #[test]
    fn in_order_packets_flush_immediately_and_rewrite_ssrc() {
        let mut buf = OutputBuffer::new(0);
        buf.add(video_pkt(1, 1000));
        buf.add(video_pkt(2, 1100));

        let flushed = buf.flush_ready();
        assert_eq!(flushed.len(), 2);
        assert!(flushed.iter().all(|p| p.header.ssrc == nal::VIDEO_SSRC));
    }

    #[test]
    fn first_flush_locks_starting_sequence_to_first_arrival() {
        // Matches the Go original: the flush pointer starts at whichever
        // sequence number happened to arrive first, not the "true" stream
        // start — there's no way to know the true start in advance.
        let mut buf = OutputBuffer::new(0);
        buf.add(video_pkt(5, 1100));
        let flushed = buf.flush_ready();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].header.sequence_number, 5);

        // A lower, out-of-order sequence number that arrives after the
        // pointer has already advanced past it won't flush on its own.
        buf.add(video_pkt(4, 1000));
        assert!(buf.flush_ready().is_empty());
    }

    #[test]
    fn stale_gap_is_skipped_after_timeout() {
        let mut buf = OutputBuffer::new(0);
        buf.add(video_pkt(1, 1000));
        assert_eq!(buf.flush_ready().len(), 1); // locks pointer at 1, advances to 2

        // seq 2 never arrives; seq 3 does, leaving a gap at the pointer.
        buf.add(video_pkt(3, 1200));
        assert!(
            buf.flush_ready().is_empty(),
            "gap is fresh, should not skip yet"
        );

        std::thread::sleep(MAX_STALE_PACKET_AGE + Duration::from_millis(20));
        let flushed = buf.flush_ready();
        assert_eq!(flushed.len(), 1);
        // Output sequence numbers are continuous (1, 2, 3, ...) regardless of
        // the gap in *input* sequence numbers — this is the packet that was
        // originally seq 3, but it's the second packet ever output.
        assert_eq!(flushed[0].header.sequence_number, 2);
    }

    #[test]
    fn video_and_audio_are_independent_streams() {
        let mut buf = OutputBuffer::new(0);
        buf.add(video_pkt(1, 1000));
        let mut audio = video_pkt(1, 2000);
        audio.header.payload_type = nal::PAYLOAD_TYPE_OPUS;
        buf.add(audio);

        let flushed = buf.flush_ready();
        assert_eq!(flushed.len(), 2);
        assert!(flushed.iter().any(|p| p.header.ssrc == nal::VIDEO_SSRC));
        assert!(flushed.iter().any(|p| p.header.ssrc == nal::AUDIO_SSRC));
    }
}
