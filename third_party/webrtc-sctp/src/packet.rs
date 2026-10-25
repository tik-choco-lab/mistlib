use std::fmt;

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::chunk::chunk_abort::ChunkAbort;
use crate::chunk::chunk_cookie_ack::ChunkCookieAck;
use crate::chunk::chunk_cookie_echo::ChunkCookieEcho;
use crate::chunk::chunk_error::ChunkError;
use crate::chunk::chunk_forward_tsn::ChunkForwardTsn;
use crate::chunk::chunk_header::*;
use crate::chunk::chunk_heartbeat::ChunkHeartbeat;
use crate::chunk::chunk_init::ChunkInit;
use crate::chunk::chunk_payload_data::ChunkPayloadData;
use crate::chunk::chunk_reconfig::ChunkReconfig;
use crate::chunk::chunk_selective_ack::ChunkSelectiveAck;
use crate::chunk::chunk_shutdown::ChunkShutdown;
use crate::chunk::chunk_shutdown_ack::ChunkShutdownAck;
use crate::chunk::chunk_shutdown_complete::ChunkShutdownComplete;
use crate::chunk::chunk_type::*;
use crate::chunk::chunk_unknown::ChunkUnknown;
use crate::chunk::Chunk;
use crate::error::{Error, Result};
use crate::util::*;

/// Packet represents an SCTP packet
///
/// An SCTP packet is composed of a common header and chunks.  A chunk
/// contains either control information or user data.
///
///
///SCTP Packet Format
/// 0                   1                   2                   3
/// 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
///+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///|                        Common Header                          |
///+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///|                          Chunk #1                             |
///+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///|                           ...                                 |
///+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///|                          Chunk #n                             |
///+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///
///
///SCTP Common Header Format
///
/// 0                   1                   2                   3
/// 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
///+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///|     Source Value Number        |     Destination Value Number |
///+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///|                      Verification Tag                         |
///+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///|                           Checksum                            |
///+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
///
/// ## Specifications
///
/// * [RFC 4960 §3]
///
/// [RFC 4960 §3]: https://tools.ietf.org/html/rfc4960#section-3
#[derive(Default, Debug)]
pub(crate) struct Packet {
    pub(crate) source_port: u16,
    pub(crate) destination_port: u16,
    pub(crate) verification_tag: u32,
    pub(crate) chunks: Vec<Box<dyn Chunk + Send + Sync>>,
}

/// makes packet printable
impl fmt::Display for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut res = format!(
            "Packet:
        source_port: {}
        destination_port: {}
        verification_tag: {}
        ",
            self.source_port, self.destination_port, self.verification_tag,
        );
        for chunk in &self.chunks {
            res += format!("Chunk: {chunk}").as_str();
        }
        write!(f, "{res}")
    }
}

pub(crate) const PACKET_HEADER_SIZE: usize = 12;

impl Packet {
    pub(crate) fn unmarshal(raw: &Bytes) -> Result<Self> {
        if raw.len() < PACKET_HEADER_SIZE {
            return Err(Error::ErrPacketRawTooSmall);
        }

        let reader = &mut raw.clone();

        let source_port = reader.get_u16();
        let destination_port = reader.get_u16();
        let verification_tag = reader.get_u32();

        #[cfg(not(fuzzing))]
        // only check for checksums when we are not fuzzing. This lets the fuzzer test the code much easier without guessing correct checksums.
        {
            let their_checksum = reader.get_u32_le();
            let our_checksum = generate_packet_checksum(raw);

            if their_checksum != our_checksum {
                return Err(Error::ErrChecksumMismatch);
            }
        }

        let mut chunks = vec![];
        let mut offset = PACKET_HEADER_SIZE;
        loop {
            // Exact match, no more chunks
            if offset == raw.len() {
                break;
            } else if offset + CHUNK_HEADER_SIZE > raw.len() {
                return Err(Error::ErrParseSctpChunkNotEnoughData);
            }

            let header = ChunkHeader::unmarshal(&raw.slice(offset..))?;
            let ct = header.typ;
            let c: Box<dyn Chunk + Send + Sync> = match ct {
                CT_INIT => Box::new(ChunkInit::unmarshal(&raw.slice(offset..))?),
                CT_INIT_ACK => Box::new(ChunkInit::unmarshal(&raw.slice(offset..))?),
                CT_ABORT => Box::new(ChunkAbort::unmarshal(&raw.slice(offset..))?),
                CT_COOKIE_ECHO => Box::new(ChunkCookieEcho::unmarshal(&raw.slice(offset..))?),
                CT_COOKIE_ACK => Box::new(ChunkCookieAck::unmarshal(&raw.slice(offset..))?),
                CT_HEARTBEAT => Box::new(ChunkHeartbeat::unmarshal(&raw.slice(offset..))?),
                CT_PAYLOAD_DATA => Box::new(ChunkPayloadData::unmarshal(&raw.slice(offset..))?),
                CT_SACK => Box::new(ChunkSelectiveAck::unmarshal(&raw.slice(offset..))?),
                CT_RECONFIG => Box::new(ChunkReconfig::unmarshal(&raw.slice(offset..))?),
                CT_FORWARD_TSN => Box::new(ChunkForwardTsn::unmarshal(&raw.slice(offset..))?),
                CT_ERROR => Box::new(ChunkError::unmarshal(&raw.slice(offset..))?),
                CT_SHUTDOWN => Box::new(ChunkShutdown::unmarshal(&raw.slice(offset..))?),
                CT_SHUTDOWN_ACK => Box::new(ChunkShutdownAck::unmarshal(&raw.slice(offset..))?),
                CT_SHUTDOWN_COMPLETE => {
                    Box::new(ChunkShutdownComplete::unmarshal(&raw.slice(offset..))?)
                }
                _ => Box::new(ChunkUnknown::unmarshal(&raw.slice(offset..))?),
            };

            // Advance by the wire length: some parsers omit optional fields or padding.
            let chunk_value_padding = get_padding_size(header.value_length());
            offset += CHUNK_HEADER_SIZE + header.value_length() + chunk_value_padding;
            chunks.push(c);
        }

        Ok(Packet {
            source_port,
            destination_port,
            verification_tag,
            chunks,
        })
    }

    pub(crate) fn marshal_to(&self, writer: &mut BytesMut) -> Result<usize> {
        // Populate static headers
        // 8-12 is Checksum which will be populated when packet is complete
        writer.put_u16(self.source_port);
        writer.put_u16(self.destination_port);
        writer.put_u32(self.verification_tag);

        // This is where the checksum will be written
        let checksum_pos = writer.len();
        writer.extend_from_slice(&[0, 0, 0, 0]);

        // Populate chunks
        for c in &self.chunks {
            c.marshal_to(writer)?;

            let padding_needed = get_padding_size(writer.len());
            if padding_needed != 0 {
                // padding needed if < 4 because we pad to 4
                writer.extend_from_slice(&[0u8; PADDING_MULTIPLE][..padding_needed]);
            }
        }

        let mut digest = ISCSI_CRC.digest();
        digest.update(writer);
        let checksum = digest.finalize();

        // Checksum is already in BigEndian
        // Using LittleEndian stops it from being flipped
        let checksum_place = &mut writer[checksum_pos..checksum_pos + 4];
        checksum_place.copy_from_slice(&checksum.to_le_bytes());

        Ok(writer.len())
    }

    pub(crate) fn marshal(&self) -> Result<Bytes> {
        let mut buf = BytesMut::with_capacity(PACKET_HEADER_SIZE);
        self.marshal_to(&mut buf)?;
        Ok(buf.freeze())
    }
}

impl Packet {
    pub(crate) fn check_packet(&self) -> Result<()> {
        // All packets must adhere to these rules

        // This is the SCTP sender's port number.  It can be used by the
        // receiver in combination with the source IP address, the SCTP
        // destination port, and possibly the destination IP address to
        // identify the association to which this packet belongs.  The port
        // number 0 MUST NOT be used.
        if self.source_port == 0 {
            return Err(Error::ErrSctpPacketSourcePortZero);
        }

        // This is the SCTP port number to which this packet is destined.
        // The receiving host will use this port number to de-multiplex the
        // SCTP packet to the correct receiving endpoint/application.  The
        // port number 0 MUST NOT be used.
        if self.destination_port == 0 {
            return Err(Error::ErrSctpPacketDestinationPortZero);
        }

        // Check values on the packet that are specific to a particular chunk type
        for c in &self.chunks {
            if let Some(ci) = c.as_any().downcast_ref::<ChunkInit>() {
                if !ci.is_ack {
                    // An INIT or INIT ACK chunk MUST NOT be bundled with any other chunk.
                    // They MUST be the only chunks present in the SCTP packets that carry
                    // them.
                    if self.chunks.len() != 1 {
                        return Err(Error::ErrInitChunkBundled);
                    }

                    // A packet containing an INIT chunk MUST have a zero Verification
                    // Tag.
                    if self.verification_tag != 0 {
                        return Err(Error::ErrInitChunkVerifyTagNotZero);
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::chunk::chunk_forward_tsn::ChunkForwardTsnStream;
    use crate::chunk::chunk_payload_data::PayloadProtocolIdentifier;
    use crate::chunk::chunk_selective_ack::GapAckBlock;
    use crate::error_cause::{OUT_OF_RESOURCE, PROTOCOL_VIOLATION};
    use crate::param::param_reconfig_response::ParamReconfigResponse;

    fn data() -> Box<dyn Chunk + Send + Sync> {
        Box::new(ChunkPayloadData {
            tsn: 101,
            stream_identifier: 3,
            stream_sequence_number: 7,
            beginning_fragment: true,
            ending_fragment: true,
            payload_type: PayloadProtocolIdentifier::Binary,
            user_data: Bytes::from_static(b"data"),
            ..Default::default()
        })
    }

    fn sack() -> Box<dyn Chunk + Send + Sync> {
        Box::new(ChunkSelectiveAck {
            cumulative_tsn_ack: 99,
            advertised_receiver_window_credit: 65536,
            gap_ack_blocks: vec![GapAckBlock { start: 2, end: 4 }],
            duplicate_tsn: vec![98],
        })
    }

    fn assert_data(chunk: &dyn Chunk) {
        let c = chunk.as_any().downcast_ref::<ChunkPayloadData>().unwrap();
        assert_eq!(c.tsn, 101);
        assert_eq!(c.stream_identifier, 3);
        assert_eq!(c.stream_sequence_number, 7);
        assert!(c.beginning_fragment && c.ending_fragment);
        assert_eq!(c.payload_type, PayloadProtocolIdentifier::Binary);
        assert_eq!(c.user_data.as_ref(), b"data");
    }

    fn assert_sack(chunk: &dyn Chunk) {
        let c = chunk.as_any().downcast_ref::<ChunkSelectiveAck>().unwrap();
        assert_eq!(c.cumulative_tsn_ack, 99);
        assert_eq!(c.advertised_receiver_window_credit, 65536);
        assert_eq!(c.gap_ack_blocks.len(), 1);
        assert_eq!((c.gap_ack_blocks[0].start, c.gap_ack_blocks[0].end), (2, 4));
        assert_eq!(c.duplicate_tsn, vec![98]);
    }

    // Preserve the supplied wire layout while Packet::marshal supplies padding and CRC32c.
    fn wire_chunk(raw: &[u8]) -> Box<dyn Chunk + Send + Sync> {
        Box::new(ChunkUnknown::unmarshal(&Bytes::copy_from_slice(raw)).unwrap())
    }

    fn roundtrip(chunks: Vec<Box<dyn Chunk + Send + Sync>>) -> Result<Packet> {
        let p = Packet {
            source_port: 5000,
            destination_port: 5000,
            verification_tag: 123,
            chunks,
        };
        Packet::unmarshal(&p.marshal()?)
    }

    #[test]
    fn test_forward_tsn_bundled_and_last() -> Result<()> {
        // RFC 3758 section 3.2: four-byte TSN followed by four-byte stream entries.
        for stream_count in 0..=2 {
            for with_sack in [false, true] {
                for with_data in [false, true] {
                    let mut chunks = vec![];
                    if with_sack {
                        chunks.push(sack());
                    }
                    let index = chunks.len();
                    chunks.push(Box::new(ChunkForwardTsn {
                        new_cumulative_tsn: 100,
                        streams: (0..stream_count)
                            .map(|i| ChunkForwardTsnStream {
                                identifier: i + 1,
                                sequence: i + 10,
                            })
                            .collect(),
                    }));
                    if with_data {
                        chunks.push(data());
                    }
                    let packet = roundtrip(chunks)?;
                    assert_eq!(packet.chunks.len(), index + 1 + usize::from(with_data));
                    let forward = packet.chunks[index]
                        .as_any()
                        .downcast_ref::<ChunkForwardTsn>()
                        .unwrap();
                    assert_eq!(forward.new_cumulative_tsn, 100);
                    assert_eq!(forward.streams.len(), stream_count as usize);
                    for (i, stream) in forward.streams.iter().enumerate() {
                        assert_eq!(stream.identifier, i as u16 + 1);
                        assert_eq!(stream.sequence, i as u16 + 10);
                    }
                    if with_sack {
                        assert_sack(packet.chunks[0].as_ref());
                    }
                    if with_data {
                        assert_data(packet.chunks[index + 1].as_ref());
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn test_error_and_abort_cause_boundaries() -> Result<()> {
        for typ in [CT_ERROR, CT_ABORT] {
            // Empty ABORT, an aligned cause, and a padded cause followed by another cause.
            for causes in [
                &[][..],
                &[0, 4, 0, 4][..],
                &[0, 13, 0, 5, b'x', 0, 0, 0, 0, 4, 0, 4][..],
            ] {
                let mut raw = BytesMut::new();
                raw.put_u8(typ.0);
                raw.put_u8(0);
                raw.put_u16((4 + causes.len()) as u16);
                raw.extend_from_slice(causes);
                // ABORT must be last in a valid packet; also exercise its parser with a suffix.
                for with_suffix in [false, true] {
                    let mut chunks = vec![sack(), wire_chunk(&raw)];
                    if with_suffix {
                        chunks.push(sack());
                    }
                    let packet = roundtrip(chunks)?;
                    assert_eq!(packet.chunks.len(), 2 + usize::from(with_suffix));
                    assert_sack(packet.chunks[0].as_ref());
                    let parsed = if typ == CT_ABORT {
                        &packet.chunks[1]
                            .as_any()
                            .downcast_ref::<ChunkAbort>()
                            .unwrap()
                            .error_causes
                    } else {
                        &packet.chunks[1]
                            .as_any()
                            .downcast_ref::<ChunkError>()
                            .unwrap()
                            .error_causes
                    };
                    assert_eq!(
                        parsed.len(),
                        if causes.is_empty() {
                            0
                        } else if causes.len() == 4 {
                            1
                        } else {
                            2
                        }
                    );
                    if let Some(last) = parsed.last() {
                        assert_eq!(last.code, OUT_OF_RESOURCE);
                    }
                    if parsed.len() == 2 {
                        assert_eq!(parsed[0].code, PROTOCOL_VIOLATION);
                        assert_eq!(parsed[0].raw.as_ref(), b"x");
                    }
                    if with_suffix {
                        assert_sack(packet.chunks[2].as_ref());
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn test_shutdown_bundled_with_sack() -> Result<()> {
        let packet = roundtrip(vec![
            Box::new(ChunkShutdown {
                cumulative_tsn_ack: 100,
            }),
            sack(),
        ])?;
        assert_eq!(packet.chunks.len(), 2);
        assert_eq!(
            packet.chunks[0]
                .as_any()
                .downcast_ref::<ChunkShutdown>()
                .unwrap()
                .cumulative_tsn_ack,
            100
        );
        assert_sack(packet.chunks[1].as_ref());
        // The total buffer has eight bytes, but the declared value has no TSN.
        assert!(ChunkShutdown::unmarshal(&Bytes::from_static(&[7, 0, 0, 4, 0, 0, 0, 0])).is_err());
        Ok(())
    }

    #[test]
    fn test_init_parameter_boundary_with_suffix() -> Result<()> {
        for param in [
            &[0xc0, 0, 0, 4][..],    // FORWARD-TSN supported
            &[0, 7, 0, 5, b'x'][..], // Unaligned final state cookie; padding is outside chunk.
        ] {
            let mut raw = BytesMut::from(
                &[
                    1, 0, 0, 20, // INIT header, followed by its fixed fields
                    0, 0, 0, 1, 0, 1, 0, 0, 0, 2, 0, 2, 0, 0, 0, 100,
                ][..],
            );
            raw[3] += param.len() as u8;
            raw.extend_from_slice(param);
            // INIT bundling is forbidden by check_packet; parsing must still stay in bounds.
            let packet = roundtrip(vec![wire_chunk(&raw), data()])?;
            assert_eq!(packet.chunks.len(), 2);
            let init = packet.chunks[0]
                .as_any()
                .downcast_ref::<ChunkInit>()
                .unwrap();
            assert_eq!(init.initiate_tag, 1);
            assert_eq!(init.initial_tsn, 100);
            assert_eq!(init.params.len(), 1);
            assert_eq!(packet.check_packet(), Err(Error::ErrInitChunkBundled));
            assert_data(packet.chunks[1].as_ref());
        }
        Ok(())
    }

    #[test]
    fn test_truncated_causes_and_sack_counts_do_not_read_suffix() {
        for typ in [CT_ABORT, CT_ERROR] {
            for cause in [
                &[0, 13, 0, 0][..], // Cause length smaller than its header.
                &[0, 13, 0, 8][..], // Cause value extends beyond this chunk.
                &[0][..],           // Partial cause header.
            ] {
                let mut raw = BytesMut::new();
                raw.put_u8(typ.0);
                raw.put_u8(0);
                raw.put_u16((CHUNK_HEADER_SIZE + cause.len()) as u16);
                raw.extend_from_slice(cause);
                raw.extend_from_slice(&[0; 16]);
                let raw = raw.freeze();
                if typ == CT_ABORT {
                    assert!(ChunkAbort::unmarshal(&raw).is_err());
                } else {
                    assert!(ChunkError::unmarshal(&raw).is_err());
                }
            }
        }
        for counts in [(1, 0), (0, 1), (u16::MAX, u16::MAX)] {
            let mut raw = BytesMut::from(&[3, 0, 0, 16, 0, 0, 0, 99, 0, 1, 0, 0][..]);
            raw.put_u16(counts.0);
            raw.put_u16(counts.1);
            raw.extend_from_slice(&[0; 16]);
            assert!(ChunkSelectiveAck::unmarshal(&raw.freeze()).is_err());
        }
    }

    #[test]
    fn test_reconfig_optional_fields_and_wire_length() -> Result<()> {
        for with_second_param in [false, true] {
            // RFC 6525 response with optional Sender's/Receiver's Next TSN fields.
            let mut raw = BytesMut::from(
                &[
                    130, 0, 0, 24, 0, 16, 0, 20, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 100, 0, 0, 0, 200,
                ][..],
            );
            if with_second_param {
                raw[3] = 36;
                raw.extend_from_slice(&[0, 16, 0, 12, 0, 0, 0, 2, 0, 0, 0, 1]);
            }
            let packet = roundtrip(vec![wire_chunk(&raw), data()])?;
            assert_eq!(packet.chunks.len(), 2);
            let reconfig = packet.chunks[0]
                .as_any()
                .downcast_ref::<ChunkReconfig>()
                .unwrap();
            let a = reconfig
                .param_a
                .as_ref()
                .unwrap()
                .as_any()
                .downcast_ref::<ParamReconfigResponse>()
                .unwrap();
            assert_eq!(a.reconfig_response_sequence_number, 1);
            assert_eq!(reconfig.param_b.is_some(), with_second_param);
            if let Some(b) = &reconfig.param_b {
                assert_eq!(
                    b.as_any()
                        .downcast_ref::<ParamReconfigResponse>()
                        .unwrap()
                        .reconfig_response_sequence_number,
                    2
                );
            }
            assert_data(packet.chunks[1].as_ref());
        }
        Ok(())
    }

    #[test]
    fn test_packet_unmarshal() -> Result<()> {
        let result = Packet::unmarshal(&Bytes::new());
        assert!(
            result.is_err(),
            "Unmarshal should fail when a packet is too small to be SCTP"
        );

        let header_only = Bytes::from_static(&[
            0x13, 0x88, 0x13, 0x88, 0x00, 0x00, 0x00, 0x00, 0x06, 0xa9, 0x00, 0xe1,
        ]);
        let pkt = Packet::unmarshal(&header_only)?;
        //assert!(result.o(), "Unmarshal failed for SCTP packet with no chunks: {}", result);
        assert_eq!(
            pkt.source_port, 5000,
            "Unmarshal passed for SCTP packet, but got incorrect source port exp: {} act: {}",
            5000, pkt.source_port
        );
        assert_eq!(
            pkt.destination_port, 5000,
            "Unmarshal passed for SCTP packet, but got incorrect destination port exp: {} act: {}",
            5000, pkt.destination_port
        );
        assert_eq!(
            pkt.verification_tag, 0,
            "Unmarshal passed for SCTP packet, but got incorrect verification tag exp: {} act: {}",
            0, pkt.verification_tag
        );

        let raw_chunk = Bytes::from_static(&[
            0x13, 0x88, 0x13, 0x88, 0x00, 0x00, 0x00, 0x00, 0x81, 0x46, 0x9d, 0xfc, 0x01, 0x00,
            0x00, 0x56, 0x55, 0xb9, 0x64, 0xa5, 0x00, 0x02, 0x00, 0x00, 0x04, 0x00, 0x08, 0x00,
            0xe8, 0x6d, 0x10, 0x30, 0xc0, 0x00, 0x00, 0x04, 0x80, 0x08, 0x00, 0x09, 0xc0, 0x0f,
            0xc1, 0x80, 0x82, 0x00, 0x00, 0x00, 0x80, 0x02, 0x00, 0x24, 0x9f, 0xeb, 0xbb, 0x5c,
            0x50, 0xc9, 0xbf, 0x75, 0x9c, 0xb1, 0x2c, 0x57, 0x4f, 0xa4, 0x5a, 0x51, 0xba, 0x60,
            0x17, 0x78, 0x27, 0x94, 0x5c, 0x31, 0xe6, 0x5d, 0x5b, 0x09, 0x47, 0xe2, 0x22, 0x06,
            0x80, 0x04, 0x00, 0x06, 0x00, 0x01, 0x00, 0x00, 0x80, 0x03, 0x00, 0x06, 0x80, 0xc1,
            0x00, 0x00,
        ]);

        Packet::unmarshal(&raw_chunk)?;

        Ok(())
    }

    #[test]
    fn test_packet_marshal() -> Result<()> {
        let header_only = Bytes::from_static(&[
            0x13, 0x88, 0x13, 0x88, 0x00, 0x00, 0x00, 0x00, 0x06, 0xa9, 0x00, 0xe1,
        ]);
        let pkt = Packet::unmarshal(&header_only)?;
        let header_only_marshaled = pkt.marshal()?;
        assert_eq!(header_only, header_only_marshaled, "Unmarshal/Marshaled header only packet did not match \nheaderOnly: {header_only:?} \nheader_only_marshaled {header_only_marshaled:?}");

        Ok(())
    }

    /*fn BenchmarkPacketGenerateChecksum(b *testing.B) {
        var data [1024]byte

        for i := 0; i < b.N; i++ {
            _ = generatePacketChecksum(data[:])
        }
    }*/
}
