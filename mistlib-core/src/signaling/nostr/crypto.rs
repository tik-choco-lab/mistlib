use super::event::NostrEvent;
use super::identity::TemporarySignalingIdentity;
use super::invite::derive_invite_secret;
use super::limits::MAX_NOSTR_EVENT_CONTENT_CHARS;
use super::signature::{sign_event, verify_event_signature};
use super::util::{hex_decode, sha256_hex};
use crate::error::{MistError, Result};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use k256::ecdh::diffie_hellman;
use k256::schnorr::SigningKey;
use k256::PublicKey;
use rand::{rngs::OsRng, RngCore};
use sha2::Sha256;

mod armor;
mod padding;

use armor::{decode_base64url_no_pad, encode_base64url_no_pad};
use padding::{pad_plaintext, unpad_plaintext};

const LEGACY_UNPADDED_HEX_PAYLOAD_VERSION: u8 = 2;
const LEGACY_PADDED_HEX_PAYLOAD_VERSION: u8 = 3;
const ARMORED_COVER_LEN: usize = 1;
const NONCE_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;
const EVEN_Y_COMPRESSED_PREFIX: u8 = 0x02;

pub trait NostrCrypto {
    fn encrypt(
        &self,
        sender: &TemporarySignalingIdentity,
        receiver_pubkey: &str,
        plaintext: &[u8],
    ) -> Result<String>;

    fn decrypt(
        &self,
        receiver: &TemporarySignalingIdentity,
        sender_pubkey: &str,
        ciphertext: &str,
    ) -> Result<Vec<u8>>;

    fn message_scope(
        &self,
        local: &TemporarySignalingIdentity,
        remote_pubkey: &str,
        room_id: &str,
    ) -> Result<String>;

    fn sign_event(
        &self,
        identity: &TemporarySignalingIdentity,
        event: &NostrEvent,
    ) -> Result<String>;

    fn verify_event(&self, event: &NostrEvent) -> Result<()>;
}

#[derive(Clone)]
pub struct InvitePskCrypto {
    key: [u8; 32],
}

impl InvitePskCrypto {
    pub fn new(invite_salt: &str, invite_code: &str) -> Self {
        Self {
            key: derive_invite_secret(invite_salt, invite_code),
        }
    }

    fn pair_key(
        &self,
        local: &TemporarySignalingIdentity,
        remote_pubkey: &str,
    ) -> Result<[u8; 32]> {
        let local_secret = SigningKey::from_bytes(local.secret_key().as_bytes())
            .map_err(|_| MistError::Signaling("invalid Nostr secret key".to_string()))?;
        let remote_public = public_key_from_xonly(remote_pubkey)?;
        let shared = diffie_hellman(local_secret.as_nonzero_scalar(), remote_public.as_affine());
        let hk = Hkdf::<Sha256>::new(Some(&self.key), shared.raw_secret_bytes().as_slice());
        let mut output = [0u8; 32];
        hk.expand(&pair_context(&local.public_key, remote_pubkey), &mut output)
            .map_err(|_| MistError::Signaling("failed to derive Nostr message key".to_string()))?;
        Ok(output)
    }

    fn decrypt_legacy_hex(
        &self,
        receiver: &TemporarySignalingIdentity,
        sender_pubkey: &str,
        ciphertext: &str,
    ) -> Result<Option<Vec<u8>>> {
        let payload = match hex_decode(ciphertext) {
            Ok(payload) => payload,
            Err(_) => return Ok(None),
        };
        if payload.len() < 1 + NONCE_LEN + GCM_TAG_LEN {
            return Ok(None);
        }
        let version = payload[0];
        if version != LEGACY_PADDED_HEX_PAYLOAD_VERSION
            && version != LEGACY_UNPADDED_HEX_PAYLOAD_VERSION
        {
            return Ok(None);
        }
        let nonce = &payload[1..1 + NONCE_LEN];
        let encrypted = &payload[1 + NONCE_LEN..];
        let key = self.pair_key(receiver, sender_pubkey)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let aad = message_aad(sender_pubkey, &receiver.public_key);
        let decrypted = cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: encrypted,
                    aad: &aad,
                },
            )
            .map_err(|_| MistError::Signaling("invalid encrypted Nostr payload".to_string()))?;
        if version == LEGACY_PADDED_HEX_PAYLOAD_VERSION {
            Ok(Some(unpad_plaintext(&decrypted)?))
        } else {
            Ok(Some(decrypted))
        }
    }
}

impl NostrCrypto for InvitePskCrypto {
    fn encrypt(
        &self,
        sender: &TemporarySignalingIdentity,
        receiver_pubkey: &str,
        plaintext: &[u8],
    ) -> Result<String> {
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let key = self.pair_key(sender, receiver_pubkey)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let aad = message_aad(&sender.public_key, receiver_pubkey);
        let padded = pad_plaintext(plaintext)?;
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &padded,
                    aad: &aad,
                },
            )
            .map_err(|_| MistError::Signaling("failed to encrypt Nostr payload".to_string()))?;
        let mut cover = [0u8; ARMORED_COVER_LEN];
        OsRng.fill_bytes(&mut cover);
        let mut payload = Vec::with_capacity(cover.len() + nonce.len() + ciphertext.len());
        payload.extend_from_slice(&cover);
        payload.extend_from_slice(&nonce);
        payload.extend_from_slice(&ciphertext);
        Ok(encode_base64url_no_pad(&payload))
    }

    fn decrypt(
        &self,
        receiver: &TemporarySignalingIdentity,
        sender_pubkey: &str,
        ciphertext: &str,
    ) -> Result<Vec<u8>> {
        if ciphertext.len() > MAX_NOSTR_EVENT_CONTENT_CHARS {
            return Err(MistError::Signaling(
                "Nostr event content is too large".to_string(),
            ));
        }
        if let Some(plaintext) = self.decrypt_legacy_hex(receiver, sender_pubkey, ciphertext)? {
            return Ok(plaintext);
        }
        let payload = decode_base64url_no_pad(ciphertext)?;
        if payload.len() < ARMORED_COVER_LEN + NONCE_LEN + GCM_TAG_LEN {
            return Err(MistError::Signaling(
                "unsupported encrypted Nostr payload".to_string(),
            ));
        }
        let nonce_start = ARMORED_COVER_LEN;
        let encrypted_start = nonce_start + NONCE_LEN;
        let nonce = &payload[nonce_start..encrypted_start];
        let encrypted = &payload[encrypted_start..];
        let key = self.pair_key(receiver, sender_pubkey)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let aad = message_aad(sender_pubkey, &receiver.public_key);
        let decrypted = cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: encrypted,
                    aad: &aad,
                },
            )
            .map_err(|_| MistError::Signaling("invalid encrypted Nostr payload".to_string()))?;
        unpad_plaintext(&decrypted)
    }

    fn message_scope(
        &self,
        local: &TemporarySignalingIdentity,
        remote_pubkey: &str,
        room_id: &str,
    ) -> Result<String> {
        let pair_key = self.pair_key(local, remote_pubkey)?;
        Ok(message_scope_hash(
            &pair_key,
            &local.public_key,
            remote_pubkey,
            room_id,
        ))
    }

    fn sign_event(
        &self,
        identity: &TemporarySignalingIdentity,
        event: &NostrEvent,
    ) -> Result<String> {
        sign_event(identity.secret_key().as_bytes(), event)
    }

    fn verify_event(&self, event: &NostrEvent) -> Result<()> {
        let expected_id = {
            let mut clone = event.clone();
            clone.refresh_id();
            clone.id
        };
        if event.id != expected_id {
            return Err(MistError::Signaling("invalid Nostr event id".to_string()));
        }
        verify_event_signature(event)
    }
}

fn message_scope_hash(pair_key: &[u8; 32], a: &str, b: &str, room_id: &str) -> String {
    let mut input = Vec::with_capacity(160 + room_id.len());
    input.extend_from_slice(b"nostr-signaling-message-scope:v1\0");
    input.extend_from_slice(pair_key);
    input.push(0);
    if a <= b {
        input.extend_from_slice(a.as_bytes());
        input.push(0);
        input.extend_from_slice(b.as_bytes());
    } else {
        input.extend_from_slice(b.as_bytes());
        input.push(0);
        input.extend_from_slice(a.as_bytes());
    }
    input.push(0);
    input.extend_from_slice(room_id.as_bytes());
    sha256_hex(&input)
}

fn public_key_from_xonly(pubkey: &str) -> Result<PublicKey> {
    let x =
        hex_decode(pubkey).map_err(|_| MistError::Signaling("invalid Nostr pubkey".to_string()))?;
    if x.len() != 32 {
        return Err(MistError::Signaling("invalid Nostr pubkey".to_string()));
    }
    let mut compressed = [0u8; 33];
    compressed[0] = EVEN_Y_COMPRESSED_PREFIX;
    compressed[1..].copy_from_slice(&x);
    PublicKey::from_sec1_bytes(&compressed)
        .map_err(|_| MistError::Signaling("invalid Nostr pubkey".to_string()))
}

fn pair_context(a: &str, b: &str) -> Vec<u8> {
    let mut context = Vec::with_capacity(120);
    context.extend_from_slice(b"nostr-signaling-ecdh-aesgcm:v2\0");
    if a <= b {
        context.extend_from_slice(a.as_bytes());
        context.push(0);
        context.extend_from_slice(b.as_bytes());
    } else {
        context.extend_from_slice(b.as_bytes());
        context.push(0);
        context.extend_from_slice(a.as_bytes());
    }
    context
}

fn message_aad(sender_pubkey: &str, receiver_pubkey: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(112);
    aad.extend_from_slice(b"nostr-signaling-message:v2\0");
    aad.extend_from_slice(sender_pubkey.as_bytes());
    aad.push(0);
    aad.extend_from_slice(receiver_pubkey.as_bytes());
    aad
}
#[cfg(test)]
mod tests;
