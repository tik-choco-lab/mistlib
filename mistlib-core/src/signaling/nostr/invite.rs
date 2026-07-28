use super::util::{sha256, sha256_hex};

pub fn derive_invite_scope(invite_salt: &str, invite_code: &str) -> String {
    let mut input = Vec::with_capacity(invite_salt.len() + invite_code.len() + 32);
    input.extend_from_slice(b"mistlib-nostr-invite-scope:v1\0");
    input.extend_from_slice(invite_salt.as_bytes());
    input.push(0);
    input.extend_from_slice(invite_code.as_bytes());
    sha256_hex(&input)
}

pub fn derive_invite_secret(invite_salt: &str, invite_code: &str) -> [u8; 32] {
    let mut input = Vec::with_capacity(invite_salt.len() + invite_code.len() + 32);
    input.extend_from_slice(b"mistlib-nostr-invite-secret:v1\0");
    input.extend_from_slice(invite_salt.as_bytes());
    input.push(0);
    input.extend_from_slice(invite_code.as_bytes());
    sha256(&input)
}

pub fn derive_room_scope(invite_secret: &[u8; 32], room_id: &str, rotation_bucket: u64) -> String {
    let mut input = Vec::with_capacity(room_id.len() + 80);
    input.extend_from_slice(b"mistlib-nostr-room-scope:v1\0");
    input.extend_from_slice(invite_secret);
    input.push(0);
    input.extend_from_slice(&rotation_bucket.to_le_bytes());
    input.push(0);
    input.extend_from_slice(room_id.as_bytes());
    sha256_hex(&input)
}

/// Derives the per-room, per-rotation-bucket "broadcast" sentinel used as the
/// `p` tag value on kind-25050 messages whose logical receiver is
/// [`NodeId::broadcast`](crate::types::NodeId::broadcast) rather than a known
/// peer. It is identical for every member of a room (so any member's
/// subscription accepts it) while remaining unlinkable across rooms and
/// rotation buckets, mirroring [`derive_room_scope`] with a distinct
/// domain-separation label.
pub fn derive_broadcast_sentinel(
    invite_secret: &[u8; 32],
    room_id: &str,
    rotation_bucket: u64,
) -> String {
    let mut input = Vec::with_capacity(room_id.len() + 80);
    input.extend_from_slice(b"mistlib-nostr-broadcast-p:v1\0");
    input.extend_from_slice(invite_secret);
    input.push(0);
    input.extend_from_slice(&rotation_bucket.to_le_bytes());
    input.push(0);
    input.extend_from_slice(room_id.as_bytes());
    sha256_hex(&input)
}

pub fn derive_topology_rank(invite_secret: &[u8; 32], room_id: &str, pubkey: &str) -> String {
    let mut input = Vec::with_capacity(room_id.len() + pubkey.len() + 80);
    input.extend_from_slice(b"mistlib-nostr-topology-rank:v1\0");
    input.extend_from_slice(invite_secret);
    input.push(0);
    input.extend_from_slice(room_id.as_bytes());
    input.push(0);
    input.extend_from_slice(pubkey.as_bytes());
    sha256_hex(&input)
}

pub fn derive_discovery_proof(
    invite_secret: &[u8; 32],
    room_id: &str,
    pubkey: &str,
    expires_at: u64,
    nonce: &str,
) -> String {
    let mut input = Vec::with_capacity(room_id.len() + pubkey.len() + nonce.len() + 96);
    input.extend_from_slice(b"mistlib-nostr-discovery-proof:v1\0");
    input.extend_from_slice(invite_secret);
    input.push(0);
    input.extend_from_slice(room_id.as_bytes());
    input.push(0);
    input.extend_from_slice(pubkey.as_bytes());
    input.push(0);
    input.extend_from_slice(&expires_at.to_le_bytes());
    input.push(0);
    input.extend_from_slice(nonce.as_bytes());
    sha256_hex(&input)
}
