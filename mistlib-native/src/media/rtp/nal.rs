//! H264 NAL unit parsing helpers, ported from mistlink/internal/rtp_utils/nal.go.

pub const PAYLOAD_TYPE_H264: u8 = 96;
pub const PAYLOAD_TYPE_OPUS: u8 = 111;
pub const PAYLOAD_TYPE_AAC: u8 = 112;

pub const VIDEO_SSRC: u32 = 0x1234_5678;
pub const AUDIO_SSRC: u32 = 0x8765_4321;

pub const MAX_TIMESTAMP_DELTA: u32 = 90_000;

pub const LOG_INTERVAL_PACKETS: u32 = 100;
pub const DUMMY_TIMESTAMP_INCREMENT: u32 = 9_000;

pub const NAL_TYPE_NON_IDR: u8 = 1;
pub const NAL_TYPE_IDR: u8 = 5;
pub const NAL_TYPE_SEI: u8 = 6;
pub const NAL_TYPE_SPS: u8 = 7;
pub const NAL_TYPE_PPS: u8 = 8;
pub const NAL_TYPE_STAP_A: u8 = 24;
pub const NAL_TYPE_FU_A: u8 = 28;
pub const NAL_MASK: u8 = 0x1F;
pub const FU_START_MASK: u8 = 0x80;
pub const FU_END_MASK: u8 = 0x40;

pub fn get_nal_type(payload: &[u8]) -> u8 {
    match payload.first() {
        Some(&b) => b & NAL_MASK,
        None => 0,
    }
}

/// Whether a depacketized H264 RTP payload carries (or starts) an IDR
/// (keyframe) NAL unit, ported from the IDR-detection portion of
/// mistlink/internal/receiver/rtp_handler.go's `ProcessVideoPacket`.
pub fn is_idr_nal(payload: &[u8], nal_type: u8) -> bool {
    match nal_type {
        NAL_TYPE_IDR => true,
        NAL_TYPE_FU_A => {
            payload.len() > 1
                && (payload[1] & FU_START_MASK) != 0
                && (payload[1] & NAL_MASK) == NAL_TYPE_IDR
        }
        NAL_TYPE_STAP_A => {
            let mut pos = 1usize;
            while pos + 2 <= payload.len() {
                let size = ((payload[pos] as usize) << 8) | payload[pos + 1] as usize;
                pos += 2;
                if pos + size > payload.len() {
                    break;
                }
                let unit = &payload[pos..pos + size];
                if let Some(&first) = unit.first() {
                    if (first & NAL_MASK) == NAL_TYPE_IDR {
                        return true;
                    }
                }
                pos += size;
            }
            false
        }
        _ => false,
    }
}

pub fn get_nal_type_name(nal_type: u8) -> String {
    match nal_type {
        NAL_TYPE_NON_IDR => "Non-IDR".to_string(),
        NAL_TYPE_IDR => "IDR".to_string(),
        NAL_TYPE_SEI => "SEI".to_string(),
        NAL_TYPE_SPS => "SPS".to_string(),
        NAL_TYPE_PPS => "PPS".to_string(),
        NAL_TYPE_STAP_A => "STAP-A".to_string(),
        NAL_TYPE_FU_A => "FU-A".to_string(),
        other => format!("Unknown({other})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_nal_type_empty_payload_returns_zero() {
        assert_eq!(get_nal_type(&[]), 0);
    }

    #[test]
    fn get_nal_type_masks_top_three_bits() {
        // top 3 bits (nal_ref_idc) should be masked off, keeping only type bits.
        assert_eq!(get_nal_type(&[0xE5]), NAL_TYPE_IDR);
        assert_eq!(get_nal_type(&[0x67]), NAL_TYPE_SPS);
    }

    #[test]
    fn get_nal_type_name_known_types() {
        assert_eq!(get_nal_type_name(NAL_TYPE_NON_IDR), "Non-IDR");
        assert_eq!(get_nal_type_name(NAL_TYPE_IDR), "IDR");
        assert_eq!(get_nal_type_name(NAL_TYPE_SEI), "SEI");
        assert_eq!(get_nal_type_name(NAL_TYPE_SPS), "SPS");
        assert_eq!(get_nal_type_name(NAL_TYPE_PPS), "PPS");
        assert_eq!(get_nal_type_name(NAL_TYPE_STAP_A), "STAP-A");
        assert_eq!(get_nal_type_name(NAL_TYPE_FU_A), "FU-A");
    }

    #[test]
    fn get_nal_type_name_unknown_type() {
        assert_eq!(get_nal_type_name(2), "Unknown(2)");
    }

    #[test]
    fn is_idr_nal_single_idr() {
        assert!(is_idr_nal(&[0x65], NAL_TYPE_IDR));
    }

    #[test]
    fn is_idr_nal_non_idr_single() {
        assert!(!is_idr_nal(&[0x61], NAL_TYPE_NON_IDR));
    }

    #[test]
    fn is_idr_nal_fu_a_start_fragment() {
        // FU indicator, FU header (start bit + IDR type)
        let payload = [0x7C, 0x85];
        assert!(is_idr_nal(&payload, NAL_TYPE_FU_A));
    }

    #[test]
    fn is_idr_nal_fu_a_non_start_fragment_is_not_idr() {
        let payload = [0x7C, 0x05];
        assert!(!is_idr_nal(&payload, NAL_TYPE_FU_A));
    }

    #[test]
    fn is_idr_nal_stap_a_contains_idr() {
        let idr_unit = [0x65, 0xAA];
        let mut payload = vec![NAL_TYPE_STAP_A];
        payload.push(0);
        payload.push(idr_unit.len() as u8);
        payload.extend_from_slice(&idr_unit);
        assert!(is_idr_nal(&payload, NAL_TYPE_STAP_A));
    }

    #[test]
    fn is_idr_nal_stap_a_without_idr() {
        let unit = [0x61, 0xAA];
        let mut payload = vec![NAL_TYPE_STAP_A];
        payload.push(0);
        payload.push(unit.len() as u8);
        payload.extend_from_slice(&unit);
        assert!(!is_idr_nal(&payload, NAL_TYPE_STAP_A));
    }
}
