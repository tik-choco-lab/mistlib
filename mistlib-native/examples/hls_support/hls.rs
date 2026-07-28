//! Live HLS playlist + segment rotation. New code (not a Go port) — feeds
//! [`super::ts::TsMuxer`] output into rolling `.ts` segments and
//! generates the matching `.m3u8`, matching what VRChat's AVPro Video player
//! (and any standard HLS client) expects for a live stream.

use std::collections::VecDeque;

use super::h264::{AccessUnit, Depacketizer};
use super::ts::TsMuxer;

pub struct SegmenterConfig {
    /// Target segment duration; segments are cut on the first keyframe at or
    /// after this many seconds have elapsed, per HLS convention.
    pub target_duration_secs: u32,
    /// How many completed segments to keep in the rolling window before the
    /// oldest is evicted (and `EXT-X-MEDIA-SEQUENCE` advances).
    pub max_segments: usize,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            target_duration_secs: 2,
            max_segments: 6,
        }
    }
}

struct Segment {
    index: u64,
    data: Vec<u8>,
    duration_secs: f64,
}

/// Consumes RTP H264 packets and produces a rolling window of MPEG-TS
/// segments plus the `.m3u8` playlist describing them.
pub struct Segmenter {
    config: SegmenterConfig,
    depacketizer: Depacketizer,
    muxer: TsMuxer,

    current: Vec<u8>,
    current_start_pts: Option<u32>,
    next_index: u64,
    media_sequence: u64,
    segments: VecDeque<Segment>,
}

impl Segmenter {
    pub fn new(config: SegmenterConfig) -> Self {
        Self {
            config,
            depacketizer: Depacketizer::new(),
            muxer: TsMuxer::new(),
            current: Vec::new(),
            current_start_pts: None,
            next_index: 0,
            media_sequence: 0,
            segments: VecDeque::new(),
        }
    }

    /// Feeds one RTP packet's H264 payload (already the RTP payload, not the
    /// full packet). Call once per RTP packet on the video track.
    pub fn push_rtp(&mut self, payload: &[u8], timestamp: u32, marker: bool) {
        if let Some(au) = self.depacketizer.push(payload, timestamp, marker) {
            self.push_access_unit(au);
        }
    }

    fn push_access_unit(&mut self, au: AccessUnit) {
        let pts = au.pts_90k;
        let is_keyframe = au.is_keyframe;

        let start_pts = *self.current_start_pts.get_or_insert(pts);
        let elapsed_secs = pts.wrapping_sub(start_pts) as f64 / 90_000.0;

        if is_keyframe
            && !self.current.is_empty()
            && elapsed_secs >= self.config.target_duration_secs as f64
        {
            self.rotate_segment(elapsed_secs);
            self.current_start_pts = Some(pts);
        }

        let ts_bytes = self.muxer.mux_access_unit(&au);
        self.current.extend_from_slice(&ts_bytes);
    }

    fn rotate_segment(&mut self, duration_secs: f64) {
        let data = std::mem::take(&mut self.current);
        self.segments.push_back(Segment {
            index: self.next_index,
            data,
            duration_secs,
        });
        self.next_index += 1;

        while self.segments.len() > self.config.max_segments {
            self.segments.pop_front();
            self.media_sequence += 1;
        }
    }

    /// Returns the raw MPEG-TS bytes for `segment{index}.ts`, if still in
    /// the rolling window.
    pub fn segment(&self, index: u64) -> Option<&[u8]> {
        self.segments
            .iter()
            .find(|s| s.index == index)
            .map(|s| s.data.as_slice())
    }

    /// Number of completed segments currently in the window.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Renders the current `.m3u8` playlist text for the rolling window.
    pub fn playlist(&self) -> String {
        let mut m3u8 = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
        m3u8.push_str(&format!(
            "#EXT-X-TARGETDURATION:{}\n",
            self.config.target_duration_secs
        ));
        m3u8.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", self.media_sequence));
        for seg in &self.segments {
            m3u8.push_str(&format!("#EXTINF:{:.3},\n", seg.duration_secs));
            m3u8.push_str(&format!("segment{}.ts\n", seg.index));
        }
        m3u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives one full access unit (single-NAL, marker set) through the
    /// segmenter's public `push_rtp` API.
    fn push_frame(seg: &mut Segmenter, nal_byte: u8, pts_90k: u32) {
        seg.push_rtp(&[nal_byte, 0xAA], pts_90k, true);
    }

    #[test]
    fn no_segment_until_a_keyframe_after_target_duration() {
        let mut seg = Segmenter::new(SegmenterConfig {
            target_duration_secs: 2,
            max_segments: 6,
        });
        push_frame(&mut seg, 0x65, 0); // keyframe, t=0
        push_frame(&mut seg, 0x61, 90_000); // non-keyframe, t=1s — not enough elapsed, not a keyframe anyway
        assert_eq!(seg.segment_count(), 0);
    }

    #[test]
    fn keyframe_after_target_duration_rotates_a_segment() {
        let mut seg = Segmenter::new(SegmenterConfig {
            target_duration_secs: 2,
            max_segments: 6,
        });
        push_frame(&mut seg, 0x65, 0);
        push_frame(&mut seg, 0x65, 3 * 90_000); // 3s later, keyframe -> rotates
        assert_eq!(seg.segment_count(), 1);
        assert!(seg.segment(0).is_some());
        assert!(!seg.segment(0).unwrap().is_empty());
    }

    #[test]
    fn playlist_lists_segments_with_correct_target_duration_and_media_sequence() {
        let mut seg = Segmenter::new(SegmenterConfig {
            target_duration_secs: 2,
            max_segments: 6,
        });
        push_frame(&mut seg, 0x65, 0);
        push_frame(&mut seg, 0x65, 3 * 90_000);
        push_frame(&mut seg, 0x65, 6 * 90_000);

        let playlist = seg.playlist();
        assert!(playlist.starts_with("#EXTM3U\n"));
        assert!(playlist.contains("#EXT-X-TARGETDURATION:2\n"));
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
        assert!(playlist.contains("segment0.ts"));
        assert!(playlist.contains("segment1.ts"));
        assert!(
            !playlist.contains("segment2.ts"),
            "segment 2 hasn't rotated in yet"
        );
    }

    #[test]
    fn rolling_window_evicts_oldest_segment_and_advances_media_sequence() {
        let mut seg = Segmenter::new(SegmenterConfig {
            target_duration_secs: 1,
            max_segments: 2,
        });
        for i in 0..4u32 {
            push_frame(&mut seg, 0x65, i * 2 * 90_000);
        }
        // 4 keyframes spaced 2s apart with a 1s target => 3 rotations, window=2
        assert_eq!(seg.segment_count(), 2);
        assert!(
            seg.segment(0).is_none(),
            "oldest segment should have been evicted"
        );
        assert!(seg.segment(1).is_some());
        assert!(seg.segment(2).is_some());
        assert!(seg.playlist().contains("#EXT-X-MEDIA-SEQUENCE:1\n"));
    }
}
