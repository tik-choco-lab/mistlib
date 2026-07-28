#[path = "../src/transport/webrtc/sdp_lines.rs"]
mod sdp_lines;

use sdp_lines::mline_signature;

const THREE_SECTION_SDP: &str = "v=0\r\n\
o=- 123456 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1 2\r\n\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:1\r\n\
a=sendrecv\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:2\r\n\
a=sendrecv\r\n";

#[test]
fn multi_section_sdp_extracts_ordered_kind_mid_pairs() {
    assert_eq!(
        mline_signature(THREE_SECTION_SDP),
        vec![
            ("application".to_string(), "0".to_string()),
            ("video".to_string(), "1".to_string()),
            ("audio".to_string(), "2".to_string()),
        ]
    );
}

#[test]
fn section_without_a_mid_line_gets_empty_mid() {
    let sdp = "v=0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=sendrecv\r\n";
    assert_eq!(
        mline_signature(sdp),
        vec![("audio".to_string(), String::new())]
    );
}

#[test]
fn empty_sdp_has_empty_signature() {
    assert!(mline_signature("").is_empty());
}

#[test]
fn identical_sdps_produce_equal_signatures() {
    assert_eq!(
        mline_signature(THREE_SECTION_SDP),
        mline_signature(THREE_SECTION_SDP)
    );
}

#[test]
fn count_mismatch_is_not_equal() {
    // Offer B (2 m-lines: application + video) vs offer C (3 m-lines: adds
    // audio) -- exactly the field failure's shape: a late answer for offer B
    // must not be mistaken for an answer to offer C.
    let two_sections = "v=0\r\n\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
a=mid:0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:1\r\n";
    assert_ne!(
        mline_signature(two_sections),
        mline_signature(THREE_SECTION_SDP)
    );
}

#[test]
fn mid_mismatch_is_not_equal() {
    let same_kinds_different_mid = "v=0\r\n\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
a=mid:0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:9\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:2\r\n";
    assert_ne!(
        mline_signature(same_kinds_different_mid),
        mline_signature(THREE_SECTION_SDP)
    );
}

#[test]
fn kind_mismatch_is_not_equal() {
    let same_mids_different_kind = "v=0\r\n\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
a=mid:0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:1\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:2\r\n";
    assert_ne!(
        mline_signature(same_mids_different_kind),
        mline_signature(THREE_SECTION_SDP)
    );
}

#[test]
fn reordered_sections_are_not_equal() {
    let reordered = "v=0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=mid:1\r\n\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
a=mid:0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:2\r\n";
    assert_ne!(
        mline_signature(reordered),
        mline_signature(THREE_SECTION_SDP)
    );
}
