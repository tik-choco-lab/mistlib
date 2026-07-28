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
    let mut out = String::with_capacity((data.len() * 8).div_ceil(5));
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

/// Upper bound on a CID string's length. Every CID `compute_cid` emits is
/// exactly 59 chars (`b` + base32 of a 36-byte CIDv1/sha2-256 envelope); the
/// generous ceiling leaves room for future codecs while still bounding the
/// filename a backend derives from an untrusted CID.
pub const MAX_CID_LEN: usize = 128;

/// True only for a well-formed multibase-`b` base32 CIDv1 string of the shape
/// [`compute_cid`] produces: a leading `b` followed by `base32lower` characters
/// (`a-z`, `2-7`) only.
///
/// This is a *security* boundary, not just a sanity check: a `BlockStore`
/// derives an on-disk filename from the CID (e.g. `base_dir.join(cid)` in the
/// native backend), and CIDs arrive verbatim from remote peers over the
/// storage protocol (`WANT`/`QUERY`/`HAVE`). The base32-lower alphabet contains
/// none of `/`, `\`, `.`, or `:`, so a string that passes this check cannot
/// contain a path separator, a `..` component, a drive prefix, or a leading
/// root — it therefore cannot escape the block directory when joined to it.
/// Backends and protocol handlers must reject any CID that fails this before
/// touching the filesystem.
pub fn is_valid_cid(cid: &str) -> bool {
    let bytes = cid.as_bytes();
    if bytes.len() < 2 || bytes.len() > MAX_CID_LEN {
        return false;
    }
    if bytes[0] != b'b' {
        return false;
    }
    // `base32lower_encode`'s alphabet: `abcdefghijklmnopqrstuvwxyz234567`.
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
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
    fn computed_cids_are_accepted() {
        // Whatever `compute_cid` emits must always pass validation, for both
        // codecs and across a range of inputs.
        for len in [0usize, 1, 32, 4096] {
            let data = vec![0xABu8; len];
            assert!(is_valid_cid(&compute_cid(&data, MULTICODEC_RAW)));
            assert!(is_valid_cid(&compute_cid(&data, MULTICODEC_DAG_CBOR)));
        }
    }

    #[test]
    fn path_traversal_cids_are_rejected() {
        // The strings a malicious peer would send to escape the block dir.
        for bad in [
            "",
            "b",
            "../../etc/passwd",
            "..\\..\\Windows\\System32\\drivers\\etc\\hosts",
            "/etc/passwd",
            "C:\\secrets.txt",
            "b../foo",
            "b..",
            "b/nested",
            "b.hidden",
            "bABC",     // uppercase is outside base32-lower
            "bfoo=bar", // padding / query chars
            "afoo",     // wrong multibase prefix
        ] {
            assert!(!is_valid_cid(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn overlong_cids_are_rejected() {
        let overlong = format!("b{}", "a".repeat(MAX_CID_LEN));
        assert!(!is_valid_cid(&overlong));
    }
}
