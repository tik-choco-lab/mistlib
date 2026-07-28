//! SPS/PPS extraction from H264 RTP payloads, ported from
//! mistlink/internal/receiver/sps_pps.go.
//!
//! The Go original also owned side effects (locking a shared bridge state,
//! starting an RTSP server once both SPS and PPS are known). Those concerns
//! belong to the RTSP/receiver layer, not this codec-parsing crate, so this
//! port keeps the pure parsing logic (`extract_sps_pps`) and leaves the
//! start-up orchestration to the caller.

use crate::media::rtp::nal;
use bytes::Bytes;

/// Result of scanning a single RTP payload for SPS/PPS NAL units.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpsPps {
    pub sps: Option<Bytes>,
    pub pps: Option<Bytes>,
}

impl SpsPps {
    fn is_empty(&self) -> bool {
        self.sps.is_none() && self.pps.is_none()
    }
}

/// Scans a single depacketized RTP payload for SPS/PPS NAL units, handling
/// single NAL units, STAP-A aggregation, and FU-A fragmentation (start
/// fragment only, matching the Go original).
pub fn extract_sps_pps(payload: &[u8], nal_type: u8) -> Option<SpsPps> {
    if payload.is_empty() {
        return None;
    }

    let result = match nal_type {
        nal::NAL_TYPE_SPS => SpsPps {
            sps: Some(Bytes::copy_from_slice(payload)),
            pps: None,
        },
        nal::NAL_TYPE_PPS => SpsPps {
            sps: None,
            pps: Some(Bytes::copy_from_slice(payload)),
        },
        nal::NAL_TYPE_STAP_A => extract_from_stap_a(payload),
        nal::NAL_TYPE_FU_A => extract_from_fu_a(payload),
        _ => SpsPps::default(),
    };

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

fn extract_from_stap_a(payload: &[u8]) -> SpsPps {
    let mut found = SpsPps::default();
    let mut pos = 1usize;

    while pos + 2 <= payload.len() {
        let size = ((payload[pos] as usize) << 8) | payload[pos + 1] as usize;
        pos += 2;
        if pos + size > payload.len() {
            break;
        }
        let unit = &payload[pos..pos + size];
        if let Some(&first) = unit.first() {
            let nt = first & nal::NAL_MASK;
            if nt == nal::NAL_TYPE_SPS && found.sps.is_none() {
                found.sps = Some(Bytes::copy_from_slice(unit));
            } else if nt == nal::NAL_TYPE_PPS && found.pps.is_none() {
                found.pps = Some(Bytes::copy_from_slice(unit));
            }
        }
        pos += size;
    }

    found
}

fn extract_from_fu_a(payload: &[u8]) -> SpsPps {
    let mut found = SpsPps::default();

    if payload.len() > 1 && (payload[1] & nal::FU_START_MASK) != 0 {
        let orig = payload[1] & nal::NAL_MASK;
        if orig == nal::NAL_TYPE_SPS || orig == nal::NAL_TYPE_PPS {
            let mut unit = Vec::with_capacity(1 + payload.len() - 2);
            unit.push((payload[0] & 0xE0) | orig);
            unit.extend_from_slice(&payload[2..]);
            if orig == nal::NAL_TYPE_SPS {
                found.sps = Some(Bytes::from(unit));
            } else {
                found.pps = Some(Bytes::from(unit));
            }
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_payload_returns_none() {
        assert_eq!(extract_sps_pps(&[], nal::NAL_TYPE_SPS), None);
    }

    #[test]
    fn single_sps_nal() {
        let payload = [0x67, 0x42, 0x00, 0x1F];
        let result = extract_sps_pps(&payload, nal::NAL_TYPE_SPS).unwrap();
        assert_eq!(result.sps.as_deref(), Some(&payload[..]));
        assert_eq!(result.pps, None);
    }

    #[test]
    fn single_pps_nal() {
        let payload = [0x68, 0xCE, 0x3C, 0x80];
        let result = extract_sps_pps(&payload, nal::NAL_TYPE_PPS).unwrap();
        assert_eq!(result.pps.as_deref(), Some(&payload[..]));
        assert_eq!(result.sps, None);
    }

    #[test]
    fn unrelated_nal_type_returns_none() {
        assert_eq!(extract_sps_pps(&[0x65, 0x01], nal::NAL_TYPE_IDR), None);
    }

    #[test]
    fn stap_a_extracts_both_sps_and_pps() {
        let sps_unit = [0x67, 0xAA, 0xBB];
        let pps_unit = [0x68, 0xCC];

        let mut payload = vec![nal::NAL_TYPE_STAP_A];
        payload.push((sps_unit.len() >> 8) as u8);
        payload.push((sps_unit.len() & 0xFF) as u8);
        payload.extend_from_slice(&sps_unit);
        payload.push((pps_unit.len() >> 8) as u8);
        payload.push((pps_unit.len() & 0xFF) as u8);
        payload.extend_from_slice(&pps_unit);

        let result = extract_sps_pps(&payload, nal::NAL_TYPE_STAP_A).unwrap();
        assert_eq!(result.sps.as_deref(), Some(&sps_unit[..]));
        assert_eq!(result.pps.as_deref(), Some(&pps_unit[..]));
    }

    #[test]
    fn stap_a_with_truncated_unit_stops_gracefully() {
        // declares a size larger than remaining bytes
        let payload = [nal::NAL_TYPE_STAP_A, 0x00, 0x10, 0x67];
        assert_eq!(extract_sps_pps(&payload, nal::NAL_TYPE_STAP_A), None);
    }

    #[test]
    fn fu_a_start_fragment_sps() {
        // FU indicator byte, FU header with start bit set and SPS type (7)
        let payload = [0x7C, 0x87, 0xAA, 0xBB];
        let result = extract_sps_pps(&payload, nal::NAL_TYPE_FU_A).unwrap();
        // reconstructed NAL header = (indicator & 0xE0) | orig_type
        assert_eq!(result.sps.as_deref(), Some(&[0x67, 0xAA, 0xBB][..]));
        assert_eq!(result.pps, None);
    }

    #[test]
    fn fu_a_non_start_fragment_is_ignored() {
        // start bit not set
        let payload = [0x7C, 0x07, 0xAA, 0xBB];
        assert_eq!(extract_sps_pps(&payload, nal::NAL_TYPE_FU_A), None);
    }

    #[test]
    fn fu_a_too_short_is_ignored() {
        let payload = [0x7C];
        assert_eq!(extract_sps_pps(&payload, nal::NAL_TYPE_FU_A), None);
    }
}
