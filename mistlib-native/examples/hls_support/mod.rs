//! Self-contained HLS output — **examples/tests support code, not part of
//! the `mistlib` library API.** Added for local playback testing (e.g.
//! VRChat's AVPro Video player, which plays HLS `.m3u8`/MPEG-TS over HTTP
//! but not RTSP or WebRTC directly), this is test-harness tooling, not
//! something a general-purpose networking library should ship as public
//! API — so it's pulled in via `#[path = "hls_support/mod.rs"]` by whichever
//! example or test needs it (`examples/hls_preview.rs`,
//! `examples/hls_preview_live.rs`, `tests/loopback_media.rs`) rather than
//! living under `src/`. (This file stays `mod.rs`-style rather than the
//! sibling `foo.rs` + `foo/` convention used elsewhere in this crate,
//! because Cargo auto-discovers any top-level `examples/*.rs` file as its
//! own example binary target — a bare `examples/hls_support.rs` with
//! submodules but no `main()` fails to build as one; nesting it under
//! `hls_support/mod.rs` avoids that.) No external processes (ffmpeg etc.)
//! and no new heavyweight dependencies — the MPEG-TS muxer, HLS
//! playlist/segmenter, and
//! HTTP server here are all hand-rolled pure Rust.
//!
//! Pipeline: `mistlib::media::stream::TrackBroadcaster`/
//! `mistlib::media::receiver::RtpBridge` (H264 RTP) → [`h264::Depacketizer`]
//! (RTP → Annex-B access units) → [`ts::TsMuxer`] (Annex-B → MPEG-TS packets)
//! → [`hls::Segmenter`] (TS packets → rolling `.ts` segments + `.m3u8`) →
//! [`server::serve`] (plain HTTP/1.1 GET server for the segments/playlist).
//!
//! Scope: video only (H264). VRChat's AVPro expects AAC audio, and this
//! pipeline only ever carries Opus from WebRTC — transcoding Opus→AAC needs
//! a codec, which is out of scope for a dependency-free test harness. The
//! playlist is audio-free, which HLS and AVPro both accept.
//!
//! See `examples/hls_preview.rs` for end-to-end usage.
//!
//! This module is compiled once per consumer (`#[path]` inclusion, not a
//! shared crate), and each consumer only uses part of the API — hence the
//! blanket `dead_code`/`unused_imports` allow below rather than pruning
//! per-binary.

#![allow(dead_code, unused_imports)]

pub mod h264;
pub mod hls;
pub mod server;
pub mod ts;

pub use h264::{AccessUnit, Depacketizer};
pub use hls::{Segmenter, SegmenterConfig};
pub use server::serve;
pub use ts::TsMuxer;
