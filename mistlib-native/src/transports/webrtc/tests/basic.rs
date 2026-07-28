use super::*;
use mistlib_core::types::{ConnectionState, NodeId};

#[tokio::test]
async fn disconnect_unknown_node_does_not_crash() {
    let t = make_transport();
    assert!(t.disconnect(&NodeId("unknown".to_string())).await.is_ok());
}

#[tokio::test]
async fn get_connection_state_unknown_is_disconnected() {
    let t = make_transport();
    assert_eq!(
        t.get_connection_state(&NodeId("nobody".to_string())),
        ConnectionState::Disconnected
    );
}

#[tokio::test]
async fn get_connected_nodes_empty_initially() {
    let t = make_transport();
    assert!(t.get_connected_nodes().is_empty());
}

#[tokio::test]
async fn connect_does_not_crash() {
    let t = make_transport();
    let result = t.connect(&NodeId("peer".to_string())).await;
    assert!(
        result.is_ok(),
        "connect should successfully initiate connection"
    );
}

#[tokio::test]
async fn connect_fails_cleanly_no_leaked_state() {
    let t = make_transport();
    let node = NodeId("peer".to_string());
    let _ = t.connect(&node).await;
    assert_eq!(
        t.get_connection_state(&node),
        ConnectionState::Connecting,
        "Node should enter Connecting state immediately after connect() is called"
    );
}

#[tokio::test]
async fn disconnect_after_connect_attempt_does_not_crash() {
    let t = make_transport();
    let node = NodeId("peer".to_string());
    let _ = t.connect(&node).await;
    assert!(t.disconnect(&node).await.is_ok());
}

#[tokio::test]
async fn repeated_disconnect_does_not_crash() {
    let t = make_transport();
    let node = NodeId("peer".to_string());
    assert!(t.disconnect(&node).await.is_ok());
    assert!(t.disconnect(&node).await.is_ok());
    assert!(t.disconnect(&node).await.is_ok());
}

#[tokio::test]
async fn send_to_unknown_node_does_not_crash() {
    use bytes::Bytes;
    use mistlib_core::types::DeliveryMethod;
    let t = make_transport();
    let result = t
        .send(
            &NodeId("nobody".to_string()),
            Bytes::from_static(b"hello"),
            DeliveryMethod::ReliableOrdered,
        )
        .await;
    assert!(
        result.is_err(),
        "sending to unknown peer should return an error"
    );
}

#[tokio::test]
async fn broadcast_with_no_connections_does_not_crash() {
    use bytes::Bytes;
    use mistlib_core::types::DeliveryMethod;
    let t = make_transport();
    assert!(t
        .broadcast(Bytes::from_static(b"hi"), DeliveryMethod::Unreliable)
        .await
        .is_ok());
}

#[tokio::test]
async fn set_media_track_handler_is_stored() {
    let t = make_transport();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    t.set_media_track_handler(tx);
    assert!(t.media_tx.lock().unwrap().is_some());
}

#[tokio::test]
async fn add_local_track_registers_a_sender() {
    use webrtc::api::media_engine::MIME_TYPE_H264;
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
    use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

    let t = make_transport();
    let peer = t
        .create_pc(NodeId("peer".to_string()))
        .await
        .expect("create_pc should succeed");

    let track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_string(),
        "stream".to_string(),
    ));

    let sender = peer.add_local_track(track).await;
    assert!(sender.is_ok(), "add_local_track should succeed: {sender:?}");
}

/// The answer we generate must contain ONLY codecs this crate can relay
/// onward (H264 video for RTSP/AVPro, Opus audio) -- otherwise a browser
/// offering VP8 first could pick it, and the relay would then have no way to
/// consume it. `register_default_codecs()` would also register
/// VP8/VP9/AV1/H265/G722/PCMU/PCMA; this checks the generated offer/answer
/// SDP never advertises any of them.
#[tokio::test]
async fn create_pc_offer_only_advertises_h264_and_opus_codecs() {
    use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;

    let t = make_transport();
    let peer = t
        .create_pc(NodeId("codec-peer".to_string()))
        .await
        .expect("create_pc should succeed");

    peer.pc
        .add_transceiver_from_kind(RTPCodecType::Video, None)
        .await
        .expect("add video transceiver should succeed");
    peer.pc
        .add_transceiver_from_kind(RTPCodecType::Audio, None)
        .await
        .expect("add audio transceiver should succeed");

    let offer = peer
        .pc
        .create_offer(None)
        .await
        .expect("create_offer should succeed");
    let sdp = offer.sdp;

    assert!(sdp.contains("H264"), "offer must advertise H264: {sdp}");
    assert!(sdp.contains("opus"), "offer must advertise Opus: {sdp}");
    for banned in [
        "VP8", "VP9", "AV1", "G722", "PCMU", "PCMA", "H265", "ulpfec",
    ] {
        assert!(
            !sdp.contains(banned),
            "offer must not advertise {banned} -- the browser could pick it and \
             the relay has no way to consume it: {sdp}"
        );
    }
}

#[tokio::test]
async fn add_track_and_renegotiate_fails_for_unknown_node() {
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
    use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

    let t = make_transport();
    let track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability::default(),
        "video".to_string(),
        "stream".to_string(),
    ));

    let result = t
        .add_track_and_renegotiate(&NodeId("nobody".to_string()), track)
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn add_track_and_renegotiate_rejects_while_first_offer_is_still_pending() {
    // `connect()` already sent an initial offer, so signaling_state is
    // HaveLocalOffer — renegotiating a *second* offer before the first is
    // answered would be invalid per WebRTC's offer/answer model. This is a
    // real precondition, not something a smarter test setup works around: a
    // full two-peer negotiation reaching Stable is exercised end-to-end (with
    // a real remote peer, not this crate's single-sided MockSignaler) by
    // mistlib-media's loopback integration test, which layers a real track
    // add + renegotiation on top of an already-Stable connection.
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
    use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

    let t = make_transport();
    let node = NodeId("peer".to_string());
    t.connect(&node).await.expect("connect should succeed");

    let track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability::default(),
        "video".to_string(),
        "stream".to_string(),
    ));

    let result = t.add_track_and_renegotiate(&node, track).await;
    assert!(
        result.is_err(),
        "renegotiating before Stable should be rejected"
    );
}
