//! End-to-end loopback proof that mistlib-native's media pipeline
//! (`media::receiver::RtpBridge`, `media::stream::TrackBroadcaster`) works
//! with a *real* WebRTC connection established via `WebRtcTransport` — not
//! just synthetic RTP packets fed directly into unit tests. Also proves the
//! examples-only `hls_support::Segmenter` (see `examples/hls_support/mod.rs`
//! for why it isn't library API) against real WebRTC-delivered RTP.
//!
//! This can't reach a real Nostr relay (no live counterpart to publish to in
//! CI/sandbox environments), so it substitutes a trivial in-process
//! `Signaler` that hands `MessageContent` directly between two
//! `WebRtcTransport` instances instead of going over Nostr. Everything
//! downstream of signaling — ICE, DTLS, SRTP, RTP — is real: two actual
//! `RTCPeerConnection`s negotiate and exchange real UDP packets on
//! localhost. `mistlib::signaling::NostrSignaler` itself is exercised
//! extensively by mistlib-native's own test suite and is not what this test
//! is proving; this test proves the piece that's new: WebRTC media track →
//! `RtpBridge`/`TrackBroadcaster` → `hls_support::Segmenter` → a real HLS
//! segment.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use mistlib::transports::webrtc::WebRtcTransport;
use mistlib_core::signaling::{MessageContent, Signaler, SignalingHandler};
use mistlib_core::transport::{NetworkEvent, NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, NodeId};
use webrtc::api::media_engine::MIME_TYPE_H264;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

use mistlib::media::receiver::RtpBridge;
use mistlib::media::stream::{RtpSink, TrackBroadcaster};

#[path = "../examples/hls_support/mod.rs"]
mod hls_support;
use hls_support::{Segmenter, SegmenterConfig};

/// Hands signaling messages directly to the peer's `SignalingHandler`,
/// standing in for a real signaling transport (Nostr, WebSocket, ...).
struct DirectSignaler {
    peer: Mutex<Option<Arc<dyn SignalingHandler>>>,
}

impl DirectSignaler {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            peer: Mutex::new(None),
        })
    }

    fn set_peer(&self, peer: Arc<dyn SignalingHandler>) {
        *self.peer.lock().unwrap() = Some(peer);
    }
}

#[async_trait]
impl Signaler for DirectSignaler {
    async fn send_signaling(
        &self,
        _to: &NodeId,
        msg: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        let peer = self.peer.lock().unwrap().clone();
        if let Some(peer) = peer {
            tokio::spawn(async move {
                let _ = peer.handle_message(msg).await;
            });
        }
        Ok(())
    }

    async fn close(&self) -> mistlib_core::error::Result<()> {
        Ok(())
    }
}

struct NoopEventHandler;
impl NetworkEventHandler for NoopEventHandler {
    fn on_event(&self, _event: NetworkEvent) {}
}

async fn wait_until<F: Fn() -> bool>(condition: F, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for condition"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// Requires real UDP ICE connectivity (STUN + connectivity checks) between two
// local RTCPeerConnections, which this development sandbox's network
// restrictions don't reliably allow (confirmed: STUN candidate gathering
// succeeds — genuine outbound UDP reaches the internet — but ICE
// connectivity checks stall and mistlib-native's own 6s connection watchdog
// kills the session before Connected is ever reached; this matches the
// pre-existing flaky ICE-timing test in mistlib-native's own suite,
// `datachannel_close_notifies_remote_immediately`). Run manually with
// `cargo test -p mistlib-native --test loopback_media -- --ignored` in an
// environment with normal UDP egress to verify the full pipeline live.
#[ignore = "needs real UDP ICE connectivity; unreliable in this sandbox, see comment"]
#[tokio::test]
async fn webrtc_track_flows_through_bridge_into_an_hls_segment() {
    let node_a = NodeId("publisher".to_string());
    let node_b = NodeId("viewer".to_string());

    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("debug"))
        .with_test_writer()
        .try_init();

    let sig_a = DirectSignaler::new();
    let sig_b = DirectSignaler::new();
    let transport_a = Arc::new(WebRtcTransport::new(sig_a.clone(), node_a.clone()));
    let transport_b = Arc::new(WebRtcTransport::new(sig_b.clone(), node_b.clone()));
    sig_a.set_peer(transport_b.clone());
    sig_b.set_peer(transport_a.clone());

    transport_a.start(Arc::new(NoopEventHandler)).await.unwrap();
    transport_b.start(Arc::new(NoopEventHandler)).await.unwrap();

    let (media_tx, mut media_rx) = tokio::sync::mpsc::unbounded_channel();
    transport_b.set_media_track_handler(media_tx);

    // A connects to B first (data channels only) to reach a Stable session,
    // matching the real topology: a viewer joins a room and connects to a
    // publishing peer, which later (or immediately) adds media tracks.
    transport_a
        .connect(&node_b)
        .await
        .expect("connect should succeed");
    wait_until(
        || transport_a.get_connection_state(&node_b) == ConnectionState::Connected,
        Duration::from_secs(10),
    )
    .await;

    // A adds a real H264 track and renegotiates.
    let video_track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            ..Default::default()
        },
        "video".to_string(),
        "mistlink".to_string(),
    ));
    transport_a
        .add_track_and_renegotiate(&node_b, video_track.clone())
        .await
        .expect("add_track_and_renegotiate should succeed");

    // webrtc-rs's on_track only fires once real RTP is actually flowing over
    // the negotiated transceiver, not merely once SDP negotiation completes
    // — so keep sending frames on A's side while waiting for B to see them.
    let sender_track = video_track.clone();
    let keep_sending = tokio::spawn(async move {
        let mut seq = 0u32;
        loop {
            send_frame(&sender_track, 0x65, seq * 3000).await;
            seq += 1;
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    });

    // B's on_track handler should fire with the remote track.
    let event = tokio::time::timeout(Duration::from_secs(15), media_rx.recv())
        .await
        .expect("timed out waiting for MediaTrackEvent")
        .expect("media channel closed unexpectedly");
    assert_eq!(event.remote_id, node_a);
    keep_sending.abort();

    // Wire B's received track into a bridge (as an `RtpSink`) via
    // TrackBroadcaster, and the bridge's paced output into a Segmenter —
    // exactly the pipeline documented in examples/hls_preview_live.rs.
    let bridge = RtpBridge::new(0);
    let broadcaster = TrackBroadcaster::new(
        event.track,
        event.pc,
        Some(bridge.clone() as Arc<dyn RtpSink>),
    );
    broadcaster.start();

    let segmenter = Arc::new(std::sync::RwLock::new(Segmenter::new(SegmenterConfig {
        target_duration_secs: 1,
        max_segments: 4,
    })));
    let mut packets = bridge.subscribe();
    let segmenter_task = segmenter.clone();
    tokio::spawn(async move {
        while let Ok(pkt) = packets.recv().await {
            if pkt.header.payload_type == mistlib::media::rtp::nal::PAYLOAD_TYPE_H264 {
                segmenter_task.write().unwrap().push_rtp(
                    &pkt.payload,
                    pkt.header.timestamp,
                    pkt.header.marker,
                );
            }
        }
    });

    // Send real RTP packets over the real WebRTC connection: a keyframe,
    // then a second keyframe ~1.5s later (past the 1s target duration) to
    // force a segment rotation.
    send_frame(&video_track, 0x65, 0).await;
    tokio::time::sleep(Duration::from_millis(300)).await; // let write_rtp process

    send_frame(&video_track, 0x65, 135_000).await; // +1.5s at 90kHz

    wait_until(
        || segmenter.read().unwrap().segment_count() >= 1,
        Duration::from_secs(10),
    )
    .await;

    let playlist = segmenter.read().unwrap().playlist();
    assert!(playlist.contains("#EXTM3U"));
    assert!(playlist.contains("segment0.ts"));

    let segment = segmenter.read().unwrap().segment(0).map(<[u8]>::to_vec);
    let segment = segment.expect("segment0 should exist");
    assert!(!segment.is_empty());
    assert_eq!(segment.len() % hls_support::ts::PACKET_SIZE, 0);
}

// Same real-ICE topology as `webrtc_track_flows_through_bridge_into_an_hls_segment`
// above, but exercises the new `WebRtcTransport::publish_local_track` API
// (`transports/webrtc/publish.rs`) instead of `add_track_and_renegotiate` --
// this is the actual entry point the cascade/SFU use case (mistl
// re-publishing a received VRChat screen share into a room) is built on.
// Proves it attaches and renegotiates a track over a *live* connection, not
// just in the SDP-only unit tests in `transports/webrtc/tests/publish.rs`.
// Deliberately doesn't re-prove the downstream RtpBridge/Segmenter pipeline
// (already covered above) -- just that `on_track` fires on the receiving
// side, which is the part `publish_local_track` is actually responsible for.
//
// This only exercises the "publish to an already-connected peer" half of the
// design (transport-level publish state + per-peer attach/renegotiate). The
// other half -- a late joiner's inbound offer triggering `create_pc`'s
// new-peer hook, plus the answer-side follow-up renegotiation added to
// `signaling::handle_offer` -- is covered by the SDP-only unit tests instead
// (`create_pc_attaches_already_published_tracks_to_a_new_peer` et al. in
// `transports/webrtc/tests/publish.rs`), not here: driving it live would
// require two back-to-back signaling messages from the publisher (the
// initial answer, then the follow-up offer) to arrive at the late joiner in
// order, which this test's `DirectSignaler` doesn't guarantee (every send is
// its own spawned task). A real signaling transport delivering both messages
// over one connection between the same sender/receiver pair (e.g. this
// crate's `NostrSignaler`) is expected to preserve that order, but
// reproducing the guarantee here would make the test about signaling
// ordering rather than about publish/attach.
#[ignore = "needs real UDP ICE connectivity; unreliable in this sandbox, see comment"]
#[tokio::test]
async fn publish_local_track_delivers_media_to_an_already_connected_peer() {
    let node_a = NodeId("publisher2".to_string());
    let node_b = NodeId("viewer2".to_string());

    let sig_a = DirectSignaler::new();
    let sig_b = DirectSignaler::new();
    let transport_a = Arc::new(WebRtcTransport::new(sig_a.clone(), node_a.clone()));
    let transport_b = Arc::new(WebRtcTransport::new(sig_b.clone(), node_b.clone()));
    sig_a.set_peer(transport_b.clone());
    sig_b.set_peer(transport_a.clone());

    transport_a.start(Arc::new(NoopEventHandler)).await.unwrap();
    transport_b.start(Arc::new(NoopEventHandler)).await.unwrap();

    let (media_tx, mut media_rx) = tokio::sync::mpsc::unbounded_channel();
    transport_b.set_media_track_handler(media_tx);

    // A connects to B first (data channels only) to reach a Stable session,
    // exactly like the topology above.
    transport_a
        .connect(&node_b)
        .await
        .expect("connect should succeed");
    wait_until(
        || transport_a.get_connection_state(&node_b) == ConnectionState::Connected,
        Duration::from_secs(10),
    )
    .await;

    // A publishes a real H264 track via the app-facing publish API instead of
    // the lower-level `add_track_and_renegotiate` -- this is what
    // `mistlib::publish_local_track(room_id, track)` (`app.rs`) delegates to
    // once a room's session is resolved.
    let video_track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            ..Default::default()
        },
        "video".to_string(),
        "mistlink".to_string(),
    ));
    transport_a
        .publish_local_track(video_track.clone())
        .await
        .expect("publish_local_track should succeed against an already-connected peer");

    let sender_track = video_track.clone();
    let keep_sending = tokio::spawn(async move {
        let mut seq = 0u32;
        loop {
            send_frame(&sender_track, 0x65, seq * 3000).await;
            seq += 1;
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    });

    let event = tokio::time::timeout(Duration::from_secs(15), media_rx.recv())
        .await
        .expect("timed out waiting for MediaTrackEvent")
        .expect("media channel closed unexpectedly");
    assert_eq!(event.remote_id, node_a);
    keep_sending.abort();
}

async fn send_frame(track: &Arc<TrackLocalStaticRTP>, nal_byte: u8, timestamp: u32) {
    let pkt = rtp::packet::Packet {
        header: rtp::header::Header {
            payload_type: 96,
            sequence_number: (timestamp / 3000) as u16,
            timestamp,
            marker: true,
            ..Default::default()
        },
        payload: Bytes::copy_from_slice(&[nal_byte, 0xAA, 0xBB, 0xCC]),
    };
    let _ = track.write_rtp_with_extensions(&pkt, &[]).await;
}
