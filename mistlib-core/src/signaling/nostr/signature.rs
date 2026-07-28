use super::event::NostrEvent;
use super::util::{hex_decode, hex_encode};
use crate::error::{MistError, Result};
use k256::schnorr::signature::hazmat::{PrehashSigner, PrehashVerifier};
use k256::schnorr::{Signature, SigningKey, VerifyingKey};
use rand::rngs::OsRng;

pub fn generate_secret_key_bytes() -> [u8; 32] {
    let signing_key = SigningKey::random(&mut OsRng);
    signing_key.to_bytes().into()
}

pub fn xonly_public_key_hex(secret_key: &[u8; 32]) -> Result<String> {
    let signing_key = signing_key_from_bytes(secret_key)?;
    Ok(hex_encode(&signing_key.verifying_key().to_bytes()))
}

pub fn sign_event(secret_key: &[u8; 32], event: &NostrEvent) -> Result<String> {
    let signing_key = signing_key_from_bytes(secret_key)?;
    let event_id = event_id_bytes(event)?;
    let signature: Signature = signing_key
        .sign_prehash(&event_id)
        .map_err(|_| MistError::Signaling("Nostr signing failed".to_string()))?;
    Ok(hex_encode(&signature.to_bytes()))
}

pub fn verify_event_signature(event: &NostrEvent) -> Result<()> {
    let event_id = event_id_bytes(event)?;
    let pubkey = hex_decode_exact(&event.pubkey, 32, "invalid Nostr pubkey")?;
    let sig = hex_decode_exact(&event.sig, 64, "invalid Nostr signature")?;
    let verifying_key = VerifyingKey::from_bytes(&pubkey)
        .map_err(|_| MistError::Signaling("invalid Nostr pubkey".to_string()))?;
    let signature = Signature::try_from(sig.as_slice())
        .map_err(|_| MistError::Signaling("invalid Nostr signature".to_string()))?;
    verifying_key
        .verify_prehash(&event_id, &signature)
        .map_err(|_| MistError::Signaling("invalid Nostr event signature".to_string()))
}

fn signing_key_from_bytes(secret_key: &[u8; 32]) -> Result<SigningKey> {
    SigningKey::from_bytes(secret_key)
        .map_err(|_| MistError::Signaling("invalid Nostr secret key".to_string()))
}

fn event_id_bytes(event: &NostrEvent) -> Result<[u8; 32]> {
    let bytes = hex_decode_exact(&event.id, 32, "invalid Nostr event id")?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn hex_decode_exact(value: &str, len: usize, label: &str) -> Result<Vec<u8>> {
    let bytes = hex_decode(value).map_err(|_| MistError::Signaling(label.to_string()))?;
    if bytes.len() != len {
        return Err(MistError::Signaling(label.to_string()));
    }
    Ok(bytes)
}
