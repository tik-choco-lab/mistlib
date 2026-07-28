use crate::error::{MistError, Result};

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub(super) fn encode_base64url_no_pad(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(BASE64URL[(b0 >> 2) as usize] as char);
        out.push(BASE64URL[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() >= 2 {
            out.push(BASE64URL[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() == 3 {
            out.push(BASE64URL[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

pub(super) fn decode_base64url_no_pad(value: &str) -> Result<Vec<u8>> {
    if value.len() % 4 == 1 {
        return Err(MistError::Signaling(
            "invalid armored Nostr payload".to_string(),
        ));
    }
    let mut out = Vec::with_capacity(value.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in value.bytes() {
        let value = base64url_value(byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        while bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    if bits > 0 && buffer != 0 {
        return Err(MistError::Signaling(
            "invalid armored Nostr payload".to_string(),
        ));
    }
    Ok(out)
}

fn base64url_value(byte: u8) -> Result<u8> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'-' => Ok(62),
        b'_' => Ok(63),
        _ => Err(MistError::Signaling(
            "invalid armored Nostr payload".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_without_padding() {
        for len in 0..64 {
            let input: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let encoded = encode_base64url_no_pad(&input);
            assert!(!encoded.contains('='));
            assert_eq!(decode_base64url_no_pad(&encoded).unwrap(), input);
        }
    }
}
