use super::*;
use crate::signaling::nostr::identity::SignalingSecretKey;
use crate::signaling::nostr::util::hex_encode;

#[test]
fn encrypt_uses_unique_random_nonces() {
    let crypto = InvitePskCrypto::new("salt", "invite");
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let bob = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([2u8; 32]),
    );
    let mut nonces = std::collections::BTreeSet::new();

    for _ in 0..16 {
        let ciphertext = crypto
            .encrypt(&alice, &bob.public_key, b"same plaintext")
            .unwrap();
        let payload = decode_base64url_no_pad(&ciphertext).unwrap();
        assert!(payload.len() >= ARMORED_COVER_LEN + NONCE_LEN + GCM_TAG_LEN);
        let nonce = payload[ARMORED_COVER_LEN..ARMORED_COVER_LEN + NONCE_LEN].to_vec();
        assert!(nonces.insert(nonce));
    }
}

#[test]
fn decrypt_accepts_legacy_v2_unpadded_payload() {
    let crypto = InvitePskCrypto::new("salt", "invite");
    let alice = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let bob = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([2u8; 32]),
    );
    let nonce = [7u8; NONCE_LEN];
    let key = crypto.pair_key(&alice, &bob.public_key).unwrap();
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let aad = message_aad(&alice.public_key, &bob.public_key);
    let plaintext = b"legacy-v2-message";
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext.as_slice(),
                aad: &aad,
            },
        )
        .unwrap();
    let mut payload = Vec::new();
    payload.push(LEGACY_UNPADDED_HEX_PAYLOAD_VERSION);
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&encrypted);
    let ciphertext = hex_encode(&payload);

    let decoded = crypto
        .decrypt(&bob, &alice.public_key, &ciphertext)
        .unwrap();
    assert_eq!(decoded, plaintext);
}
