//! SDP `sprop-parameter-sets` extraction, ported from
//! mistlink/internal/receiver/sdp.go (`ExtractSPSPPSFromSDP`).
//!
//! The Go original also pushed the decoded SPS/PPS directly into an
//! `RTPBridge`. That side effect is left to the caller here; this module
//! only extracts and decodes the parameter sets found in the SDP text.

use base64::Engine;

use crate::media::rtp::sps_pps::SpsPps;
use crate::media::MediaError;

/// Scans SDP text for `a=fmtp:... sprop-parameter-sets=<sps>,<pps>` lines and
/// returns every successfully decoded SPS/PPS pair, in the order found.
///
/// Lines with malformed or undecodable parameter sets are skipped, matching
/// the Go original's behavior of logging a warning and continuing.
pub fn extract_sps_pps_from_sdp(sdp: &str) -> Vec<SpsPps> {
    sdp.lines()
        .filter_map(|line| decode_fmtp_line(line.trim()))
        .collect()
}

fn decode_fmtp_line(line: &str) -> Option<SpsPps> {
    if !line.starts_with("a=fmtp:") {
        return None;
    }

    let (_, rest) = line.split_once("sprop-parameter-sets=")?;
    let params = rest.split(';').next().unwrap_or(rest);
    let mut sets = params.split(',');

    let sps_b64 = sets.next()?;
    let pps_b64 = sets.next()?;

    let sps = decode_param(sps_b64).ok()?;
    let pps = decode_param(pps_b64).ok()?;

    if sps.is_empty() || pps.is_empty() {
        return None;
    }

    Some(SpsPps {
        sps: Some(sps.into()),
        pps: Some(pps.into()),
    })
}

fn decode_param(value: &str) -> Result<Vec<u8>, MediaError> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(MediaError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    #[test]
    fn extracts_single_fmtp_line() {
        let sps = [0x67, 0x42, 0x00, 0x1F];
        let pps = [0x68, 0xCE, 0x3C, 0x80];
        let sdp = format!(
            "v=0\r\na=fmtp:96 sprop-parameter-sets={},{};profile-level-id=42001F\r\n",
            b64(&sps),
            b64(&pps)
        );

        let results = extract_sps_pps_from_sdp(&sdp);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].sps.as_deref(), Some(&sps[..]));
        assert_eq!(results[0].pps.as_deref(), Some(&pps[..]));
    }

    #[test]
    fn ignores_lines_without_sprop_parameter_sets() {
        let sdp = "v=0\r\na=fmtp:96 profile-level-id=42001F\r\n";
        assert!(extract_sps_pps_from_sdp(sdp).is_empty());
    }

    #[test]
    fn ignores_non_fmtp_lines() {
        let sdp = "m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
        assert!(extract_sps_pps_from_sdp(sdp).is_empty());
    }

    #[test]
    fn skips_invalid_base64() {
        let sdp = "a=fmtp:96 sprop-parameter-sets=not-base64!!,also-bad!!\r\n";
        assert!(extract_sps_pps_from_sdp(sdp).is_empty());
    }

    #[test]
    fn skips_when_only_one_parameter_set_present() {
        let sdp = format!("a=fmtp:96 sprop-parameter-sets={}\r\n", b64(&[0x67]));
        assert!(extract_sps_pps_from_sdp(&sdp).is_empty());
    }

    #[test]
    fn handles_multiple_fmtp_lines() {
        let sps1 = [0x67, 0x01];
        let pps1 = [0x68, 0x02];
        let sps2 = [0x67, 0x03];
        let pps2 = [0x68, 0x04];
        let sdp = format!(
            "a=fmtp:96 sprop-parameter-sets={},{}\r\na=fmtp:97 sprop-parameter-sets={},{}\r\n",
            b64(&sps1),
            b64(&pps1),
            b64(&sps2),
            b64(&pps2)
        );

        let results = extract_sps_pps_from_sdp(&sdp);
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].sps.as_deref(), Some(&sps2[..]));
    }
}
