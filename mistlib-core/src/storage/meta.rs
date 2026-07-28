//! Shared validation and key-encoding rules for `MetaStore` (SPEC-17).
//!
//! Both platform implementations (OPFS on wasm, plain files on native) store
//! one file per key, so the rules that keep a key filesystem-safe and a value
//! reasonably sized live here in core rather than being re-implemented per
//! platform.

use crate::error::{MistError, Result};
use std::fmt::Write as _;

/// Max key length in UTF-8 bytes. Keys are namespacing identifiers, not data.
pub const MAX_META_KEY_BYTES: usize = 512;

/// Max value size: 1 MiB. Generous enough for app state JSON (the KV exists
/// so same-origin apps can move mutable state out of the ~5MB shared
/// localStorage quota), but small enough to push bulk data toward the
/// content-addressed store — put the payload in `storage_add` and its CID in
/// the KV instead.
pub const MAX_META_VALUE_BYTES: usize = 1_048_576;

pub fn validate_meta_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(MistError::Internal("meta key must not be empty".into()));
    }
    if key.len() > MAX_META_KEY_BYTES {
        return Err(MistError::Internal(format!(
            "meta key exceeds {} bytes ({})",
            MAX_META_KEY_BYTES,
            key.len()
        )));
    }
    Ok(())
}

pub fn validate_meta_value(len: usize) -> Result<()> {
    if len > MAX_META_VALUE_BYTES {
        return Err(MistError::Internal(format!(
            "meta value exceeds {} bytes ({})",
            MAX_META_VALUE_BYTES, len
        )));
    }
    Ok(())
}

/// Injective key → filename encoding: `m_` + lowercase hex of the key's
/// UTF-8 bytes. Hex sidesteps path traversal, OS-reserved characters, and
/// case-insensitive filesystems in one move; the `m_` prefix keeps meta
/// files visually distinct from CIDv1 block files (base32, leading `b`),
/// with which they can never collide anyway.
pub fn encode_meta_key(key: &str) -> String {
    let mut out = String::with_capacity(2 + key.len() * 2);
    out.push_str("m_");
    for b in key.as_bytes() {
        // Writing to a String is infallible.
        let _ = write!(out, "{:02x}", b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_meta_key_rejects_empty() {
        assert!(validate_meta_key("").is_err());
    }

    #[test]
    fn test_validate_meta_key_boundary() {
        let max = "k".repeat(MAX_META_KEY_BYTES);
        assert!(validate_meta_key(&max).is_ok());
        let over = "k".repeat(MAX_META_KEY_BYTES + 1);
        assert!(validate_meta_key(&over).is_err());
    }

    #[test]
    fn test_validate_meta_key_counts_utf8_bytes_not_chars() {
        // 3 bytes per char: 171 chars = 513 bytes > 512.
        let over = "あ".repeat(171);
        assert!(validate_meta_key(&over).is_err());
    }

    #[test]
    fn test_validate_meta_value_boundary() {
        assert!(validate_meta_value(MAX_META_VALUE_BYTES).is_ok());
        assert!(validate_meta_value(MAX_META_VALUE_BYTES + 1).is_err());
    }

    #[test]
    fn test_encode_meta_key_is_hex_only() {
        let encoded = encode_meta_key("a/b\\c:d..e");
        assert!(encoded.starts_with("m_"));
        assert!(encoded[2..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn test_encode_meta_key_is_injective_for_lookalike_keys() {
        // Keys that would collide under naive sanitization must not collide
        // under hex encoding.
        assert_ne!(encode_meta_key("a/b"), encode_meta_key("a_b"));
        assert_ne!(encode_meta_key("a/b"), encode_meta_key("a\\b"));
        assert_ne!(encode_meta_key("A"), encode_meta_key("a"));
    }
}
