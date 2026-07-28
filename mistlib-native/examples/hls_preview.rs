//! Standalone HLS preview server, for testing the `output/` pipeline (RTP
//! H264 → MPEG-TS → HLS) end-to-end against a real player — VRChat's AVPro
//! Video included — without needing a live WebRTC/Nostr publisher.
//!
//! This example feeds the segmenter a synthetic H264 stream (tiny fabricated
//! SPS/PPS/IDR/P NAL units, not a real decodable picture) so it's runnable
//! with zero external setup: `cargo run -p mistlib-media --example
//! hls_preview`, then open `http://127.0.0.1:8080/stream.m3u8` in VLC or
//! paste it into an AVPro Video URL field in VRChat. Because the NAL payloads
//! are fake, no player will actually decode a picture from them — the goal
//! is to prove the *transport* (MPEG-TS muxing, segment rotation, m3u8
//! playlist, HTTP serving) works, not to preview a real video.
//!
//! For a real stream received over an actual Nostr-signaled WebRTC
//! connection (not synthetic), see `examples/hls_preview_live.rs` — it wires
//! this same `Segmenter` up to `mistlib-native`'s `NostrSignaler` +
//! `WebRtcTransport` and has been verified live against a public relay
//! (`wss://relay.damus.io`).
//!
//! The HLS/MPEG-TS stack itself (`hls_support`) is examples-only support
//! code, not part of the `mistlib` library API — see
//! `examples/hls_support/mod.rs` for why.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[path = "hls_support/mod.rs"]
mod hls_support;
use hls_support::{serve, Segmenter, SegmenterConfig};

const ADDR: &str = "127.0.0.1:8080";
const FRAME_INTERVAL: Duration = Duration::from_millis(100); // ~10fps
const KEYFRAME_EVERY: u32 = 20; // one keyframe every ~2s at 10fps

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let segmenter = Arc::new(RwLock::new(Segmenter::new(SegmenterConfig::default())));

    let addr: SocketAddr = ADDR.parse().expect("valid socket address");
    let server_segmenter = segmenter.clone();
    tokio::spawn(async move {
        if let Err(err) = serve(addr, server_segmenter).await {
            eprintln!("hls server error: {err}");
        }
    });

    println!("HLS preview server running.");
    println!("Playlist URL (paste into VRChat's AVPro Video URL field, or open in VLC):");
    println!("  http://{ADDR}/stream.m3u8");
    println!("Generating a synthetic H264 stream (not a real decodable picture) — Ctrl+C to stop.");

    let mut pts: u32 = 0;
    let mut frame_index: u32 = 0;
    let mut interval = tokio::time::interval(FRAME_INTERVAL);
    loop {
        interval.tick().await;

        let is_keyframe = frame_index.is_multiple_of(KEYFRAME_EVERY);
        // Fabricated single-NAL RTP payload: NAL header byte + a few filler
        // bytes. Type 5 = IDR (keyframe), type 1 = non-IDR.
        let nal_type: u8 = if is_keyframe { 5 } else { 1 };
        let payload = [0x60 | nal_type, 0xAA, 0xBB, 0xCC];

        segmenter.write().unwrap().push_rtp(&payload, pts, true);

        pts = pts.wrapping_add(9_000); // 90kHz clock / 10fps
        frame_index += 1;
    }
}
