use super::super::limits::MAX_NOSTR_SIGNALING_PLAINTEXT_BYTES;
use crate::error::{MistError, Result};
use rand::{rngs::OsRng, RngCore};

const PADDED_PLAINTEXT_LEN_BYTES: usize = 4;
const RANDOM_PADDING_TARGET_BYTES: [usize; 4] = [1024, 2048, 4096, 8192];
const OVERFLOW_PADDING_BLOCK_BYTES: usize = 1024;

pub(super) fn pad_plaintext(plaintext: &[u8]) -> Result<Vec<u8>> {
    if plaintext.len() > MAX_NOSTR_SIGNALING_PLAINTEXT_BYTES {
        return Err(MistError::Signaling(
            "Nostr signaling payload is too large".to_string(),
        ));
    }
    let plaintext_len = u32::try_from(plaintext.len())
        .map_err(|_| MistError::Signaling("Nostr payload is too large to pad".to_string()))?;
    let body_len = PADDED_PLAINTEXT_LEN_BYTES + plaintext.len();
    let padded_len = random_padded_len(body_len);
    let mut padded = Vec::with_capacity(padded_len);
    padded.extend_from_slice(&plaintext_len.to_be_bytes());
    padded.extend_from_slice(plaintext);
    padded.resize(padded_len, 0);
    OsRng.fill_bytes(&mut padded[body_len..]);
    Ok(padded)
}

pub(super) fn unpad_plaintext(padded: &[u8]) -> Result<Vec<u8>> {
    if padded.len() < PADDED_PLAINTEXT_LEN_BYTES {
        return Err(MistError::Signaling(
            "invalid padded Nostr payload".to_string(),
        ));
    }
    let mut len_bytes = [0u8; PADDED_PLAINTEXT_LEN_BYTES];
    len_bytes.copy_from_slice(&padded[..PADDED_PLAINTEXT_LEN_BYTES]);
    let plaintext_len = u32::from_be_bytes(len_bytes) as usize;
    if plaintext_len > MAX_NOSTR_SIGNALING_PLAINTEXT_BYTES {
        return Err(MistError::Signaling(
            "Nostr signaling payload is too large".to_string(),
        ));
    }
    let end = PADDED_PLAINTEXT_LEN_BYTES
        .checked_add(plaintext_len)
        .ok_or_else(|| MistError::Signaling("invalid padded Nostr payload".to_string()))?;
    if end > padded.len() {
        return Err(MistError::Signaling(
            "invalid padded Nostr payload".to_string(),
        ));
    }
    Ok(padded[PADDED_PLAINTEXT_LEN_BYTES..end].to_vec())
}

fn random_padded_len(body_len: usize) -> usize {
    let mut candidates = [0usize; RANDOM_PADDING_TARGET_BYTES.len()];
    let mut candidate_count = 0usize;
    for target in RANDOM_PADDING_TARGET_BYTES {
        if body_len <= target {
            candidates[candidate_count] = target;
            candidate_count += 1;
        }
    }

    if candidate_count == 0 {
        return body_len.next_multiple_of(OVERFLOW_PADDING_BLOCK_BYTES);
    }

    let index = (OsRng.next_u32() as usize) % candidate_count;
    candidates[index]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn padding_uses_random_cover_sizes_for_small_payloads() {
        let mut lengths = BTreeSet::new();
        for _ in 0..64 {
            lengths.insert(pad_plaintext(b"small signaling payload").unwrap().len());
        }

        assert!(
            lengths.len() > 1,
            "padding should not always pick the same cover size"
        );
        assert!(lengths
            .iter()
            .all(|len| RANDOM_PADDING_TARGET_BYTES.contains(len)));
    }

    #[test]
    fn oversized_payloads_still_round_trip() {
        let plaintext = vec![7u8; 3000];
        let padded = pad_plaintext(&plaintext).unwrap();

        assert_eq!(padded.len() % OVERFLOW_PADDING_BLOCK_BYTES, 0);
        assert_eq!(unpad_plaintext(&padded).unwrap(), plaintext);
    }
}
