//! Live version of `hls_preview.rs`: joins a real Nostr signaling room via
//! mistlib-native's `NostrSignaler`/`WebRtcTransport`, waits for a publishing
//! peer to offer a media track (matching mistlink's own sender→viewer
//! topology — a publisher pre-adds its OBS/WHIP track before offering, then
//! joins the room and sends offers to viewers), and bridges whatever H264 it
//! receives into an HLS playlist at `http://<addr>/stream.m3u8` for VRChat's
//! AVPro Video (or VLC, or any HLS client) to play.
//!
//! The HLS/MPEG-TS stack itself (`hls_support`) is examples-only support
//! code, not part of the `mistlib` library API — see
//! `examples/hls_support/mod.rs` for why.
//!
//! ```sh
//! # Defaults join room "mistlink-demo" using mistlib-core's default Nostr
//! # relay discovery. Override via env vars for a specific deployment:
//! MISTLINK_ROOM_ID=my-room \
//! MISTLINK_NOSTR_RELAYS=wss://relay.example.com \
//! MISTLINK_HLS_ADDR=127.0.0.1:8080 \
//! cargo run -p mistlib-native --example hls_preview_live
//! ```
//!
//! Then have a real publisher (e.g. a mistlink Go `sender` instance, or
//! another mistlib-native peer that calls `WebRtcTransport::connect` after
//! adding an H264 track to its `Peer`) join the same room, and open
//! `http://<addr>/stream.m3u8` in a player.
//!
//! Known limitation, discovered while building this: mistlib-native's
//! `WebRtcTransport::handle_offer` always creates a fresh `RTCPeerConnection`
//! for any incoming offer rather than distinguishing "initial connect" from
//! "renegotiation" — so a peer that's *already* connected to us and then
//! tries to add a track and renegotiate (`WebRtcTransport::add_track_and_renegotiate`)
//! will cause us to tear down and recreate the session rather than
//! gracefully renegotiate in place. This example works around that by only
//! expecting tracks that arrive as part of the *initial* offer (i.e. the
//! publisher adds its track before calling `connect`/sending its first
//! offer, exactly like mistlink's Go `sender` does in `pc_factory.go`).

use std::env;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use mistlib::signaling::NostrSignaler;
use mistlib::transports::webrtc::{MediaTrackEvent, WebRtcTransport};
use mistlib_core::config::NostrSignalingConfig;
use mistlib_core::signaling::{MessageContent, SignalingHandler};
use mistlib_core::transport::{NetworkEvent, NetworkEventHandler, Transport};
use mistlib_core::types::NodeId;

use mistlib::media::receiver::RtpBridge;
use mistlib::media::rtp::nal;
use mistlib::media::stream::{RtpSink, TrackBroadcaster};

#[path = "hls_support/mod.rs"]
mod hls_support;
use hls_support::{serve, Segmenter, SegmenterConfig};

struct NoopEventHandler;
impl NetworkEventHandler for NoopEventHandler {
    fn on_event(&self, _event: NetworkEvent) {}
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("MISTLINK_LOG"))
        .try_init();

    let local_id = env::var("MISTLINK_NODE_ID")
        .unwrap_or_else(|_| format!("hls-viewer-{}", std::process::id()));
    let room_id = env::var("MISTLINK_ROOM_ID").unwrap_or_else(|_| "mistlink-demo".to_string());
    let relays: Vec<String> = env::var("MISTLINK_NOSTR_RELAYS")
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let addr: SocketAddr = env::var("MISTLINK_HLS_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()?;

    let node_id = NodeId(local_id.clone());
    let nostr_config = NostrSignalingConfig {
        relays,
        ..Default::default()
    };
    let signaler = Arc::new(NostrSignaler::new(node_id.clone(), nostr_config));
    let transport = Arc::new(WebRtcTransport::new(signaler.clone(), node_id.clone()));
    transport.set_room_id(room_id.clone());

    let (media_tx, mut media_rx) = tokio::sync::mpsc::unbounded_channel::<MediaTrackEvent>();
    transport.set_media_track_handler(media_tx);
    transport.start(Arc::new(NoopEventHandler)).await?;

    let (sig_tx, mut sig_rx) = tokio::sync::mpsc::channel::<MessageContent>(1024);
    signaler.connect(sig_tx).await?;
    let signaling_transport = transport.clone();
    tokio::spawn(async move {
        while let Some(msg) = sig_rx.recv().await {
            if let Err(err) = signaling_transport.handle_message(msg).await {
                eprintln!("signaling handle error: {err}");
            }
        }
    });

    transport.announce_to_room().await?;

    let segmenter = Arc::new(RwLock::new(Segmenter::new(SegmenterConfig::default())));
    let server_segmenter = segmenter.clone();
    tokio::spawn(async move {
        if let Err(err) = serve(addr, server_segmenter).await {
            eprintln!("hls server error: {err}");
        }
    });

    println!("Node ID: {local_id}");
    println!("Room: {room_id}");
    println!("Playlist URL (paste into VRChat's AVPro Video URL field, or open in VLC):");
    println!("  http://{addr}/stream.m3u8");
    println!("Waiting for a publishing peer to join and offer video via Nostr signaling...");

    while let Some(event) = media_rx.recv().await {
        println!("Received media track from {}", event.remote_id.0);

        let bridge = RtpBridge::new(0);
        let broadcaster = TrackBroadcaster::new(
            event.track,
            event.pc,
            Some(bridge.clone() as Arc<dyn RtpSink>),
        );
        broadcaster.start();

        let mut packets = bridge.subscribe();
        let segmenter = segmenter.clone();
        tokio::spawn(async move {
            while let Ok(pkt) = packets.recv().await {
                if pkt.header.payload_type == nal::PAYLOAD_TYPE_H264 {
                    segmenter.write().unwrap().push_rtp(
                        &pkt.payload,
                        pkt.header.timestamp,
                        pkt.header.marker,
                    );
                }
            }
        });
    }

    Ok(())
}
