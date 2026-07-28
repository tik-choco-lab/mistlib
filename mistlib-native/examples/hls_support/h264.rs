//! RTP H264 (RFC 6184) depacketization into Annex-B access units, for feeding
//! into the MPEG-TS muxer. New code (not a Go port) — mistlink used pion's
//! `TrackRemote`/RTP payloader for sending, but never depacketized H264 RTP
//! back into Annex-B on the receive side, since it always relayed raw RTP.

use bytes::Bytes;

use mistlib::media::rtp::nal;

/// One decodable unit of video: all NAL units sharing a timestamp, ready to
/// be wrapped in Annex-B start codes.
pub struct AccessUnit {
    pub nals: Vec<Bytes>,
    pub pts_90k: u32,
    pub is_keyframe: bool,
}

impl AccessUnit {
    /// Concatenates all NALs with `00 00 00 01` Annex-B start codes.
    pub fn to_annex_b(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.nals.iter().map(|n| n.len() + 4).sum());
        for nal_unit in &self.nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal_unit);
        }
        out
    }
}

/// Reassembles RTP H264 packets (single NAL, STAP-A, or FU-A fragments) into
/// [`AccessUnit`]s, one per RTP marker bit (end-of-frame).
#[derive(Default)]
pub struct Depacketizer {
    au_nals: Vec<Bytes>,
    au_pts: Option<u32>,
    au_is_keyframe: bool,
    fu_buffer: Vec<u8>,
    fu_active: bool,
}

impl Depacketizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one RTP packet's depacketized H264 payload. Returns a completed
    /// [`AccessUnit`] once the marker bit signals the end of a frame.
    pub fn push(&mut self, payload: &[u8], timestamp: u32, marker: bool) -> Option<AccessUnit> {
        if payload.is_empty() {
            return None;
        }

        self.au_pts.get_or_insert(timestamp);
        let nal_type = nal::get_nal_type(payload);

        match nal_type {
            nal::NAL_TYPE_STAP_A => self.push_stap_a(payload),
            nal::NAL_TYPE_FU_A => self.push_fu_a(payload),
            _ => self.push_single(payload, nal_type),
        }

        if marker && !self.au_nals.is_empty() {
            let nals = std::mem::take(&mut self.au_nals);
            let pts = self.au_pts.take().unwrap_or(timestamp);
            let is_keyframe = std::mem::take(&mut self.au_is_keyframe);
            self.fu_buffer.clear();
            self.fu_active = false;
            return Some(AccessUnit {
                nals,
                pts_90k: pts,
                is_keyframe,
            });
        }

        None
    }

    fn push_single(&mut self, payload: &[u8], nal_type: u8) {
        if nal::is_idr_nal(payload, nal_type) {
            self.au_is_keyframe = true;
        }
        self.au_nals.push(Bytes::copy_from_slice(payload));
    }

    fn push_stap_a(&mut self, payload: &[u8]) {
        let mut pos = 1usize;
        while pos + 2 <= payload.len() {
            let size = ((payload[pos] as usize) << 8) | payload[pos + 1] as usize;
            pos += 2;
            if pos + size > payload.len() {
                break;
            }
            let unit = &payload[pos..pos + size];
            if let Some(&first) = unit.first() {
                if (first & nal::NAL_MASK) == nal::NAL_TYPE_IDR {
                    self.au_is_keyframe = true;
                }
            }
            self.au_nals.push(Bytes::copy_from_slice(unit));
            pos += size;
        }
    }

    fn push_fu_a(&mut self, payload: &[u8]) {
        if payload.len() < 2 {
            return;
        }
        let indicator = payload[0];
        let fu_header = payload[1];
        let start = (fu_header & nal::FU_START_MASK) != 0;
        let end = (fu_header & nal::FU_END_MASK) != 0;
        let orig_type = fu_header & nal::NAL_MASK;

        if start {
            self.fu_buffer.clear();
            self.fu_buffer.push((indicator & 0xE0) | orig_type);
            self.fu_active = true;
        }
        if !self.fu_active {
            // Started mid-fragment (e.g. after a dropped packet); nothing
            // sane to reconstruct, so drop until the next start fragment.
            return;
        }
        self.fu_buffer.extend_from_slice(&payload[2..]);

        if end {
            if orig_type == nal::NAL_TYPE_IDR {
                self.au_is_keyframe = true;
            }
            self.au_nals
                .push(Bytes::from(std::mem::take(&mut self.fu_buffer)));
            self.fu_active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_nal_with_marker_produces_access_unit() {
        let mut d = Depacketizer::new();
        let au = d.push(&[0x65, 0xAA, 0xBB], 1000, true).unwrap();
        assert_eq!(au.nals.len(), 1);
        assert!(au.is_keyframe);
        assert_eq!(au.pts_90k, 1000);
    }

    #[test]
    fn without_marker_no_access_unit_yet() {
        let mut d = Depacketizer::new();
        assert!(d.push(&[0x61, 0xAA], 1000, false).is_none());
    }

    #[test]
    fn fu_a_fragments_reassemble_into_one_nal() {
        let mut d = Depacketizer::new();
        // start fragment: indicator, header(start+IDR type=5)
        assert!(d.push(&[0x7C, 0x85, 0xAA], 2000, false).is_none());
        // end fragment
        let au = d.push(&[0x7C, 0x45, 0xBB, 0xCC], 2000, true).unwrap();
        assert_eq!(au.nals.len(), 1);
        assert!(au.is_keyframe);
        // reconstructed header = (indicator & 0xE0) | orig_type(5)
        assert_eq!(&au.nals[0][..], &[0x65, 0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn stap_a_splits_into_multiple_nals() {
        let mut d = Depacketizer::new();
        let unit_a = [0x67, 0x01];
        let unit_b = [0x65, 0x02];
        let mut payload = vec![nal::NAL_TYPE_STAP_A];
        payload.push(0);
        payload.push(unit_a.len() as u8);
        payload.extend_from_slice(&unit_a);
        payload.push(0);
        payload.push(unit_b.len() as u8);
        payload.extend_from_slice(&unit_b);

        let au = d.push(&payload, 3000, true).unwrap();
        assert_eq!(au.nals.len(), 2);
        assert!(au.is_keyframe);
    }

    #[test]
    fn to_annex_b_prefixes_each_nal_with_start_code() {
        let au = AccessUnit {
            nals: vec![
                Bytes::from_static(&[0x67, 0x01]),
                Bytes::from_static(&[0x65, 0x02]),
            ],
            pts_90k: 0,
            is_keyframe: true,
        };
        let annex_b = au.to_annex_b();
        assert_eq!(
            annex_b,
            vec![0, 0, 0, 1, 0x67, 0x01, 0, 0, 0, 1, 0x65, 0x02]
        );
    }
}
