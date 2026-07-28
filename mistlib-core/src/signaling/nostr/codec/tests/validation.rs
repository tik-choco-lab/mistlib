use super::*;

#[test]
fn wrong_room_is_rejected() {
    let (_raw, codec, crypto) = config();
    let identity = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let event = build_discovery_event(&codec, &crypto, &identity, "secret-room").unwrap();

    assert!(decode_discovery_event(&codec, &crypto, &event, "other").is_err());
}

#[test]
fn tampered_signature_is_rejected() {
    let (_raw, codec, crypto) = config();
    let identity = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let mut event = build_discovery_event(&codec, &crypto, &identity, "room").unwrap();
    // Overwriting with a fixed value would be a no-op ~1/256 of the time
    // (BIP340 signatures carry a random nonce, so any fixed prefix is
    // eventually produced legitimately) -- flip relative to the actual
    // prefix so the signature is always genuinely corrupted.
    let tampered = if event.sig.starts_with("00") {
        "11"
    } else {
        "00"
    };
    event.sig.replace_range(0..2, tampered);

    assert!(decode_discovery_event(&codec, &crypto, &event, "room").is_err());
}

#[test]
fn tampered_pubkey_is_rejected() {
    let (_raw, codec, crypto) = config();
    let identity = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let mut event = build_discovery_event(&codec, &crypto, &identity, "room").unwrap();
    event.pubkey = "00".repeat(32);
    event.refresh_id();

    assert!(decode_discovery_event(&codec, &crypto, &event, "room").is_err());
}

#[test]
fn tampered_event_id_is_rejected() {
    let (_raw, codec, crypto) = config();
    let identity = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let mut event = build_discovery_event(&codec, &crypto, &identity, "room").unwrap();
    event.id.replace_range(0..2, "00");

    assert!(decode_discovery_event(&codec, &crypto, &event, "room").is_err());
}

#[test]
fn far_future_expiration_is_rejected() {
    let (_raw, codec, crypto) = config();
    let identity = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let mut event = build_discovery_event(&codec, &crypto, &identity, "room").unwrap();
    let far_future = codec.expires_at() + 10_000;
    for tag in &mut event.tags {
        if tag.first().map(String::as_str) == Some(TAG_EXPIRATION) {
            tag[1] = far_future.to_string();
        }
    }
    event.refresh_id();
    event.sig = crypto.sign_event(&identity, &event).unwrap();

    assert!(decode_discovery_event(&codec, &crypto, &event, "room").is_err());
}

#[test]
fn stale_created_at_is_rejected_even_with_future_expiration() {
    let (_raw, codec, crypto) = config();
    let identity = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );
    let mut event = build_discovery_event(&codec, &crypto, &identity, "room").unwrap();
    event.created_at = now_unix_seconds()
        .saturating_sub(codec.ttl_seconds)
        .saturating_sub(codec.max_clock_skew_seconds)
        .saturating_sub(1);
    event.refresh_id();
    event.sig = crypto.sign_event(&identity, &event).unwrap();

    assert!(decode_discovery_event(&codec, &crypto, &event, "room").is_err());
}

#[test]
fn max_clock_skew_seconds_defaults_to_300_when_absent() {
    let config: NostrSignalingConfig = serde_json::from_str("{}").unwrap();

    assert_eq!(config.max_clock_skew_seconds, 300);
}

#[test]
fn custom_max_clock_skew_seconds_widens_future_timestamp_acceptance() {
    let config: NostrSignalingConfig =
        serde_json::from_str(r#"{"maxClockSkewSeconds": 900}"#).unwrap();
    assert_eq!(config.max_clock_skew_seconds, 900);

    let codec = NostrCodecConfig::from_config(&config);
    let crypto = InvitePskCrypto::new(&config.invite_salt, &config.invite_code);
    let identity = TemporarySignalingIdentity::from_secret_key(
        SignalingSecretKey::from_bytes_for_tests([1u8; 32]),
    );

    let mut accepted_event = build_discovery_event(&codec, &crypto, &identity, "room").unwrap();
    accepted_event.created_at = now_unix_seconds() + 400;
    accepted_event.refresh_id();
    accepted_event.sig = crypto.sign_event(&identity, &accepted_event).unwrap();
    assert!(decode_discovery_event(&codec, &crypto, &accepted_event, "room").is_ok());

    let mut rejected_event = build_discovery_event(&codec, &crypto, &identity, "room").unwrap();
    rejected_event.created_at = now_unix_seconds() + 1_000;
    rejected_event.refresh_id();
    rejected_event.sig = crypto.sign_event(&identity, &rejected_event).unwrap();
    assert!(decode_discovery_event(&codec, &crypto, &rejected_event, "room").is_err());
}
