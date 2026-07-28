//! MPEG-TS muxing (ISO/IEC 13818-1), pure Rust, no external processes. New
//! code (not a Go port) — mistlink never muxed MPEG-TS on the receive side
//! (`internal/sender/mpegts_to_rtp.go` only demuxed MPEG-TS *into* RTP, the
//! opposite direction). Scope: single H264 video elementary stream, no audio.

use super::h264::AccessUnit;

pub const PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;
pub const PAT_PID: u16 = 0x0000;
pub const PMT_PID: u16 = 0x1000;
pub const VIDEO_PID: u16 = 0x0101;
const VIDEO_STREAM_TYPE: u8 = 0x1B; // H264
const PROGRAM_NUMBER: u16 = 1;

/// Muxes H264 access units into MPEG-TS packets. Emits PAT/PMT before every
/// keyframe access unit (cheap and simple — a real broadcast muxer would
/// rate-limit this, but repeating it every keyframe keeps players that join
/// mid-stream in sync at minimal cost for a test/preview server).
pub struct TsMuxer {
    pat_cc: u8,
    pmt_cc: u8,
    video_cc: u8,
}

impl Default for TsMuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl TsMuxer {
    pub fn new() -> Self {
        Self {
            pat_cc: 0,
            pmt_cc: 0,
            video_cc: 0,
        }
    }

    /// Muxes one access unit, returning a byte buffer that is always a
    /// multiple of [`PACKET_SIZE`].
    pub fn mux_access_unit(&mut self, au: &AccessUnit) -> Vec<u8> {
        let mut out = Vec::new();

        if au.is_keyframe {
            out.extend_from_slice(&self.pat_packet());
            out.extend_from_slice(&self.pmt_packet());
        }

        let annex_b = au.to_annex_b();
        let pes = build_pes(&annex_b, au.pts_90k);

        let mut offset = 0;
        let mut first = true;
        while offset < pes.len() {
            let random_access = first && au.is_keyframe;
            let pcr = random_access.then(|| (au.pts_90k as u64 * 300, 0u16));
            let (packet, consumed) = write_ts_packet(
                VIDEO_PID,
                first,
                self.video_cc,
                random_access,
                pcr,
                &pes[offset..],
            );
            out.extend_from_slice(&packet);
            offset += consumed;
            self.video_cc = self.video_cc.wrapping_add(1) & 0x0F;
            first = false;
        }

        out
    }

    fn pat_packet(&mut self) -> [u8; PACKET_SIZE] {
        let section = build_pat_section();
        let packet = write_psi_packet(PAT_PID, self.pat_cc, &section);
        self.pat_cc = self.pat_cc.wrapping_add(1) & 0x0F;
        packet
    }

    fn pmt_packet(&mut self) -> [u8; PACKET_SIZE] {
        let section = build_pmt_section();
        let packet = write_psi_packet(PMT_PID, self.pmt_cc, &section);
        self.pmt_cc = self.pmt_cc.wrapping_add(1) & 0x0F;
        packet
    }
}

fn write_psi_packet(pid: u16, cc: u8, section: &[u8]) -> [u8; PACKET_SIZE] {
    let mut packet = [0xFFu8; PACKET_SIZE];
    packet[0] = SYNC_BYTE;
    packet[1] = 0x40 | (((pid >> 8) & 0x1F) as u8); // payload_unit_start_indicator=1
    packet[2] = (pid & 0xFF) as u8;
    packet[3] = 0x10 | (cc & 0x0F); // adaptation_field_control=payload only
    packet[4] = 0x00; // pointer_field
    let n = section.len().min(PACKET_SIZE - 5);
    packet[5..5 + n].copy_from_slice(&section[..n]);
    packet
}

/// Writes one TS packet carrying up to 184 bytes of `payload`, padding with
/// an adaptation field (stuffing, and optionally PCR/random-access-indicator
/// on the first packet of a keyframe) so the packet is always exactly
/// [`PACKET_SIZE`] bytes. Returns the packet and how many payload bytes it
/// consumed.
fn write_ts_packet(
    pid: u16,
    pusi: bool,
    cc: u8,
    random_access: bool,
    pcr: Option<(u64, u16)>,
    payload: &[u8],
) -> ([u8; PACKET_SIZE], usize) {
    let mut mandatory_adaptation = Vec::new();
    if random_access || pcr.is_some() {
        let mut flags = 0u8;
        if random_access {
            flags |= 0x40;
        }
        if pcr.is_some() {
            flags |= 0x10;
        }
        mandatory_adaptation.push(flags);
        if let Some((base, ext)) = pcr {
            mandatory_adaptation.extend_from_slice(&encode_pcr(base, ext));
        }
    }

    let mut packet = [0u8; PACKET_SIZE];
    packet[0] = SYNC_BYTE;
    packet[1] = ((pusi as u8) << 6) | (((pid >> 8) & 0x1F) as u8);
    packet[2] = (pid & 0xFF) as u8;

    let no_adaptation_capacity = PACKET_SIZE - 4;
    let take;
    let mut pos = 4;

    if mandatory_adaptation.is_empty() && payload.len() >= no_adaptation_capacity {
        // Fills (or overflows) the packet exactly — no adaptation field
        // (and crucially, no length byte for one) needed.
        take = no_adaptation_capacity;
        packet[3] = 0x10 | (cc & 0x0F); // adaptation_field_control = payload only
    } else {
        // Need an adaptation field either way: for the mandatory PCR/random-
        // access flags, or just to stuff the packet out to PACKET_SIZE when
        // the payload runs out early. Its own length byte must be reserved
        // from the payload budget too.
        let capacity = no_adaptation_capacity - 1 - mandatory_adaptation.len();
        take = payload.len().min(capacity);
        let stuff_len = capacity - take;

        let mut body = mandatory_adaptation;
        body.extend(std::iter::repeat_n(0xFFu8, stuff_len));

        packet[3] = 0x30 | (cc & 0x0F); // adaptation_field_control = both
        packet[pos] = body.len() as u8;
        pos += 1;
        packet[pos..pos + body.len()].copy_from_slice(&body);
        pos += body.len();
    }

    packet[pos..pos + take].copy_from_slice(&payload[..take]);
    pos += take;
    debug_assert_eq!(pos, PACKET_SIZE);

    (packet, take)
}

fn build_pes(payload: &[u8], pts_90k: u32) -> Vec<u8> {
    let mut pes = Vec::with_capacity(payload.len() + 19);
    pes.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0]); // start code + video stream id
    pes.extend_from_slice(&[0x00, 0x00]); // PES_packet_length = 0 (unbounded; valid for video)
    pes.push(0x80); // '10' marker + flags (no scrambling/priority/alignment/copyright)
    pes.push(0x80); // PTS_DTS_flags = '10' (PTS only)
    pes.push(0x05); // PES_header_data_length = 5 (PTS only)
    pes.extend_from_slice(&encode_pts(0b0010, pts_90k as u64));
    pes.extend_from_slice(payload);
    pes
}

fn encode_pts(prefix: u8, pts: u64) -> [u8; 5] {
    let pts = pts & 0x1_FFFF_FFFF;
    [
        (prefix << 4) | ((((pts >> 30) & 0x07) as u8) << 1) | 1,
        ((pts >> 22) & 0xFF) as u8,
        ((((pts >> 15) & 0x7F) as u8) << 1) | 1,
        ((pts >> 7) & 0xFF) as u8,
        (((pts & 0x7F) as u8) << 1) | 1,
    ]
}

fn encode_pcr(base: u64, ext: u16) -> [u8; 6] {
    let base = base & 0x1_FFFF_FFFF;
    let ext = ext & 0x1FF;
    [
        ((base >> 25) & 0xFF) as u8,
        ((base >> 17) & 0xFF) as u8,
        ((base >> 9) & 0xFF) as u8,
        ((base >> 1) & 0xFF) as u8,
        (((base & 0x1) as u8) << 7) | 0x7E | (((ext >> 8) & 0x1) as u8),
        (ext & 0xFF) as u8,
    ]
}

fn build_pat_section() -> Vec<u8> {
    let mut section = vec![0x00u8]; // table_id: program_association_section
    section.push(0);
    section.push(0); // placeholder for section_length
    let payload_start = section.len();
    section.extend_from_slice(&1u16.to_be_bytes()); // transport_stream_id
    section.push(0xC1); // reserved(2)='11' + version(5)=0 + current_next_indicator=1
    section.push(0); // section_number
    section.push(0); // last_section_number
    section.extend_from_slice(&PROGRAM_NUMBER.to_be_bytes());
    section.extend_from_slice(&(0xE000u16 | PMT_PID).to_be_bytes());

    finalize_psi_section(section, payload_start)
}

fn build_pmt_section() -> Vec<u8> {
    let mut section = vec![0x02u8]; // table_id: TS_program_map_section
    section.push(0);
    section.push(0); // placeholder for section_length
    let payload_start = section.len();
    section.extend_from_slice(&PROGRAM_NUMBER.to_be_bytes());
    section.push(0xC1); // reserved + version=0 + current_next_indicator=1
    section.push(0); // section_number
    section.push(0); // last_section_number
    section.extend_from_slice(&(0xE000u16 | VIDEO_PID).to_be_bytes()); // PCR_PID
    section.extend_from_slice(&0xF000u16.to_be_bytes()); // program_info_length = 0
    section.push(VIDEO_STREAM_TYPE);
    section.extend_from_slice(&(0xE000u16 | VIDEO_PID).to_be_bytes()); // elementary_PID
    section.extend_from_slice(&0xF000u16.to_be_bytes()); // ES_info_length = 0

    finalize_psi_section(section, payload_start)
}

/// Patches in `section_length` (from `payload_start` through the end of
/// `section`, plus the 4-byte CRC) and appends the MPEG-2 CRC32.
fn finalize_psi_section(mut section: Vec<u8>, payload_start: usize) -> Vec<u8> {
    let section_length = (section.len() - payload_start) as u16 + 4;
    section[1] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
    section[2] = (section_length & 0xFF) as u8;
    let crc = crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn parse_pid(packet: &[u8]) -> u16 {
        (((packet[1] & 0x1F) as u16) << 8) | packet[2] as u16
    }

    fn parse_cc(packet: &[u8]) -> u8 {
        packet[3] & 0x0F
    }

    #[test]
    fn every_188_byte_chunk_starts_with_sync_byte() {
        let mut muxer = TsMuxer::new();
        let au = AccessUnit {
            nals: vec![Bytes::from_static(&[0x65; 300])], // forces multi-packet PES
            pts_90k: 90_000,
            is_keyframe: true,
        };
        let out = muxer.mux_access_unit(&au);
        assert_eq!(out.len() % PACKET_SIZE, 0);
        assert!(out.len() >= PACKET_SIZE);
        for chunk in out.chunks(PACKET_SIZE) {
            assert_eq!(chunk[0], SYNC_BYTE);
        }
    }

    #[test]
    fn keyframe_access_unit_emits_pat_then_pmt_first() {
        let mut muxer = TsMuxer::new();
        let au = AccessUnit {
            nals: vec![Bytes::from_static(&[0x65, 0x01])],
            pts_90k: 0,
            is_keyframe: true,
        };
        let out = muxer.mux_access_unit(&au);

        let pat = &out[0..PACKET_SIZE];
        let pmt = &out[PACKET_SIZE..PACKET_SIZE * 2];
        assert_eq!(parse_pid(pat), PAT_PID);
        assert_eq!(pat[5], 0x00, "PAT table_id");
        assert_eq!(parse_pid(pmt), PMT_PID);
        assert_eq!(pmt[5], 0x02, "PMT table_id");
    }

    #[test]
    fn non_keyframe_access_unit_has_no_pat_pmt() {
        let mut muxer = TsMuxer::new();
        let au = AccessUnit {
            nals: vec![Bytes::from_static(&[0x61, 0x01])],
            pts_90k: 0,
            is_keyframe: false,
        };
        let out = muxer.mux_access_unit(&au);
        assert_eq!(parse_pid(&out[0..PACKET_SIZE]), VIDEO_PID);
    }

    #[test]
    fn video_continuity_counter_increments_across_packets_and_access_units() {
        let mut muxer = TsMuxer::new();
        let au1 = AccessUnit {
            nals: vec![Bytes::from_static(&[0x65; 300])],
            pts_90k: 0,
            is_keyframe: true,
        };
        let out1 = muxer.mux_access_unit(&au1);
        let video_packets: Vec<&[u8]> = out1
            .chunks(PACKET_SIZE)
            .filter(|p| parse_pid(p) == VIDEO_PID)
            .collect();
        assert!(
            video_packets.len() >= 2,
            "300-byte payload should span multiple TS packets"
        );
        assert_eq!(parse_cc(video_packets[0]), 0);
        assert_eq!(parse_cc(video_packets[1]), 1);

        let au2 = AccessUnit {
            nals: vec![Bytes::from_static(&[0x61, 0x02])],
            pts_90k: 3000,
            is_keyframe: false,
        };
        let out2 = muxer.mux_access_unit(&au2);
        let next_video_cc = parse_cc(&out2[0..PACKET_SIZE]);
        assert_eq!(next_video_cc as usize, video_packets.len() % 16);
    }

    #[test]
    fn pat_and_pmt_sections_pass_crc_check() {
        let pat = build_pat_section();
        let (body, crc_bytes) = pat.split_at(pat.len() - 4);
        let expected = u32::from_be_bytes(crc_bytes.try_into().unwrap());
        assert_eq!(crc32_mpeg2(body), expected);

        let pmt = build_pmt_section();
        let (body, crc_bytes) = pmt.split_at(pmt.len() - 4);
        let expected = u32::from_be_bytes(crc_bytes.try_into().unwrap());
        assert_eq!(crc32_mpeg2(body), expected);
    }
}
