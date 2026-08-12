#[path = "../src/transport/webrtc/sctp_limit.rs"]
mod sctp_limit;

use sctp_limit::effective_message_limit;

#[test]
fn missing_negotiated_limit_preserves_config_limit() {
    assert_eq!(effective_message_limit(65_536, None), 65_536);
}

#[test]
fn zero_negotiated_limit_preserves_config_limit() {
    assert_eq!(effective_message_limit(65_536, Some(0)), 65_536);
}

#[test]
fn smaller_negotiated_limit_clamps_config_limit() {
    assert_eq!(effective_message_limit(65_536, Some(32_768)), 32_768);
}

#[test]
fn larger_negotiated_limit_does_not_expand_config_limit() {
    assert_eq!(effective_message_limit(65_536, Some(131_072)), 65_536);
}

#[test]
fn equal_negotiated_limit_preserves_config_limit() {
    assert_eq!(effective_message_limit(65_536, Some(65_536)), 65_536);
}
