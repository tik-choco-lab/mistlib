//! Ported from mistlink/internal/stream/{stream_manager,forwarder,peer_connection,obs_track}.go.
//!
//! The Go original identified tracks/receivers by raw pointer (`*webrtc.TrackRemote`,
//! peer connections by string ID stored separately). Here, `TrackRemote`s are
//! keyed by their `TrackRemote::id()` string (unique per negotiated m-line),
//! which is simpler than pointer-keying in Rust and has the same semantics.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_remote::TrackRemote;

use crate::media::stream::broadcaster::TrackBroadcaster;
use crate::media::stream::sink::RtpSink;

type CloseHandler = Box<dyn FnOnce() + Send + Sync>;

/// Central registry of publisher tracks (from OBS/WHIP) and viewer peer
/// connections, wiring new tracks to existing viewers and vice versa.
#[derive(Default)]
pub struct StreamManager {
    obs_tracks: RwLock<HashMap<String, Arc<TrackRemote>>>,
    broadcasters: RwLock<HashMap<String, Arc<TrackBroadcaster>>>,
    /// receiver_id -> set of obs track ids already forwarded to it.
    forwarded: RwLock<HashMap<String, HashSet<String>>>,
    close_handlers: RwLock<HashMap<String, Vec<CloseHandler>>>,
}

impl StreamManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn register_close_handler<F: FnOnce() + Send + Sync + 'static>(
        &self,
        id: &str,
        handler: F,
    ) {
        self.close_handlers
            .write()
            .unwrap()
            .entry(id.to_string())
            .or_default()
            .push(Box::new(handler));
    }

    pub fn has_obs_tracks(&self) -> bool {
        !self.obs_tracks.read().unwrap().is_empty()
    }

    /// Registers a newly published OBS/WHIP track, starting a broadcaster for
    /// it if one doesn't already exist. `sink` is an optional RTSP loopback
    /// consumer (see `RtpSink`).
    pub fn add_obs_track(
        self: &Arc<Self>,
        track: Arc<TrackRemote>,
        pc: Arc<RTCPeerConnection>,
        sink: Option<Arc<dyn RtpSink>>,
    ) {
        let track_id = track.id();

        let is_new = {
            let mut broadcasters = self.broadcasters.write().unwrap();
            if broadcasters.contains_key(&track_id) {
                false
            } else {
                let broadcaster = TrackBroadcaster::new(track.clone(), pc, sink);
                broadcaster.start();
                broadcasters.insert(track_id.clone(), broadcaster);
                true
            }
        };
        if is_new {
            self.obs_tracks
                .write()
                .unwrap()
                .insert(track_id, track.clone());
        }
    }

    pub fn remove_obs_track(&self, track_id: &str) {
        self.obs_tracks.write().unwrap().remove(track_id);
        if let Some(b) = self.broadcasters.write().unwrap().remove(track_id) {
            b.stop();
        }
    }

    /// Creates a local relay track for `track` on `receiver_pc`, wires it to
    /// the matching broadcaster, and marks it forwarded for `receiver_id`.
    /// Renegotiation (offer/answer) is the caller's responsibility — this
    /// mirrors `ForwardTrackToReceiver`/`ForwardOBSTracksToReceiver` in the Go
    /// original, which is deliberately silent about signaling so callers can
    /// batch multiple track additions into one offer.
    pub async fn forward_track_to_receiver(
        self: &Arc<Self>,
        track: &Arc<TrackRemote>,
        receiver_pc: &Arc<RTCPeerConnection>,
        receiver_id: &str,
    ) -> Result<(), webrtc::Error> {
        let local_track = Arc::new(TrackLocalStaticRTP::new(
            track.codec().capability,
            track.id(),
            track.stream_id(),
        ));

        receiver_pc.add_track(local_track.clone()).await?;
        self.subscribe_to_track(track, local_track, receiver_id)
            .await;

        self.forwarded
            .write()
            .unwrap()
            .entry(receiver_id.to_string())
            .or_default()
            .insert(track.id());

        Ok(())
    }

    /// Forwards every OBS track not yet forwarded to `receiver_id`. Returns
    /// the list of newly forwarded track ids so the caller can decide whether
    /// a renegotiation offer is needed (mirrors the Go `skipOffer` flag by
    /// simply leaving offer/answer entirely to the caller).
    pub async fn forward_obs_tracks_to_receiver(
        self: &Arc<Self>,
        receiver_id: &str,
        receiver_pc: &Arc<RTCPeerConnection>,
    ) -> Vec<String> {
        let to_add: Vec<Arc<TrackRemote>> = {
            let obs_tracks = self.obs_tracks.read().unwrap();
            let forwarded = self.forwarded.read().unwrap();
            let already = forwarded.get(receiver_id);
            obs_tracks
                .values()
                .filter(|t| already.is_none_or(|s| !s.contains(&t.id())))
                .cloned()
                .collect()
        };

        let mut added = Vec::new();
        for track in to_add {
            match self
                .forward_track_to_receiver(&track, receiver_pc, receiver_id)
                .await
            {
                Ok(()) => added.push(track.id()),
                Err(err) => {
                    tracing::error!("error forwarding track to {receiver_id}: {err}");
                }
            }
        }
        added
    }

    async fn subscribe_to_track(
        &self,
        remote_track: &Arc<TrackRemote>,
        local_track: Arc<TrackLocalStaticRTP>,
        receiver_id: &str,
    ) {
        let broadcaster = self
            .broadcasters
            .read()
            .unwrap()
            .get(&remote_track.id())
            .cloned();
        match broadcaster {
            Some(b) => b.add_receiver(receiver_id.to_string(), local_track).await,
            None => tracing::warn!(
                "no broadcaster found for track {} when adding receiver {receiver_id}",
                remote_track.id()
            ),
        }
    }

    /// Cleans up all forwarding state and broadcaster subscriptions for a
    /// disconnected receiver, and runs any handlers registered via
    /// `register_close_handler`.
    pub async fn remove_receiver(&self, receiver_id: &str) {
        self.forwarded.write().unwrap().remove(receiver_id);

        let broadcasters: Vec<Arc<TrackBroadcaster>> = self
            .broadcasters
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for b in broadcasters {
            b.remove_receiver(receiver_id).await;
        }

        let handlers = self
            .close_handlers
            .write()
            .unwrap()
            .remove(receiver_id)
            .unwrap_or_default();
        for handler in handlers {
            handler();
        }
    }
}
