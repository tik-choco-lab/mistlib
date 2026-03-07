use sha2::{Digest, Sha256};
use unsigned_varint::encode as varint_encode;

pub const MULTICODEC_RAW: u64 = 0x55;
pub const MULTICODEC_DAG_CBOR: u64 = 0x71;
const CID_VERSION: u64 = 0x01;
const MULTIHASH_SHA2_256: u64 = 0x12;
const MULTIHASH_SHA2_256_LEN: u8 = 32;

fn encode_varint(value: u64, buf: &mut Vec<u8>) {
    let mut v = varint_encode::u64_buffer();
    buf.extend_from_slice(varint_encode::u64(value, &mut v));
}

fn base32lower_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity((data.len() * 8 + 4) / 5);
    let mut buffer: u64 = 0;
    let mut bits_left: u32 = 0;

    for &byte in data {
        buffer = (buffer << 8) | byte as u64;
        bits_left += 8;
        while bits_left >= 5 {
            bits_left -= 5;
            let idx = ((buffer >> bits_left) & 0x1F) as usize;
            out.push(ALPHABET[idx] as char);
        }
    }
    if bits_left > 0 {
        let idx = ((buffer << (5 - bits_left)) & 0x1F) as usize;
        out.push(ALPHABET[idx] as char);
    }
    out
}

#[allow(dead_code)]
pub fn base32lower_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: [i8; 256] = {
        let mut a = [-1; 256];
        let mut i = 0;
        while i < 32 {
            let ch = b"abcdefghijklmnopqrstuvwxyz234567"[i];
            a[ch as usize] = i as i8;
            i += 1;
        }
        a
    };

    let mut buffer: u64 = 0;
    let mut bits_left: u32 = 0;
    let mut out = Vec::with_capacity(s.len() * 5 / 8);

    for b in s.as_bytes() {
        let val = ALPHABET[*b as usize];
        if val < 0 {
            return None;
        }
        buffer = (buffer << 5) | (val as u64);
        bits_left += 5;
        if bits_left >= 8 {
            bits_left -= 8;
            out.push((buffer >> bits_left) as u8);
        }
    }
    Some(out)
}

pub fn compute_cid(data: &[u8], codec: u64) -> String {
    let hash = Sha256::digest(data);
    let mut cid = Vec::with_capacity(4 + 32);
    encode_varint(CID_VERSION, &mut cid);
    encode_varint(codec, &mut cid);
    encode_varint(MULTIHASH_SHA2_256, &mut cid);
    cid.push(MULTIHASH_SHA2_256_LEN);
    cid.extend_from_slice(&hash);
    format!("b{}", base32lower_encode(&cid))
}

pub fn verify_cid(cid_str: &str, data: &[u8], expected_codec: u64) -> bool {
    compute_cid(data, expected_codec) == cid_str
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cid_deterministic() {
        let data = b"hello world";
        let c1 = compute_cid(data, MULTICODEC_RAW);
        let c2 = compute_cid(data, MULTICODEC_RAW);
        assert_eq!(c1, c2);
        assert!(c1.starts_with('b'));
    }

    #[test]
    fn test_base32_roundtrip() {
        let original = b"cid-test-data";
        let encoded = base32lower_encode(original);
        let decoded = base32lower_decode(&encoded).unwrap();
        assert_eq!(original.to_vec(), decoded);
    }
}
