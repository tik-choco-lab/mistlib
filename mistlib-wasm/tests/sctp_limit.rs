#[path = "../src/transport/webrtc/sctp_limit.rs"]
mod sctp_limit;

use sctp_limit::effective_message_limit;

#[test]
fn negotiated_limit_only_clamps_the_configured_limit_downward() {
    let cases = [
        ("missing", None, 65_536),
        ("zero", Some(0), 65_536),
        ("smaller", Some(32_768), 32_768),
        ("larger", Some(131_072), 65_536),
    ];

    for (name, negotiated, expected) in cases {
        assert_eq!(
            effective_message_limit(65_536, negotiated),
            expected,
            "case: {name}"
        );
    }
}
