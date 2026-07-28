//! Native-side counterpart of `mistlib-wasm`'s `publish_local_track`/
//! `unpublish_local_track` (`mistlib-wasm/src/transport/webrtc.rs`): lets an
//! app publish a local media track (e.g. a received screen-share re-encoded
//! for cascade/SFU relay -- a native peer joins a room as an ordinary member
//! and rebroadcasts a track it received from elsewhere, such as a VRChat
//! screen share, to every other peer in that room) into a `WebRtcTransport`'s
//! room. Every peer already connected gets the track attached and its
//! connection renegotiated; every peer that connects afterward gets it
//! automatically via the new-peer hook in `connection::create_pc`, with no
//! extra action required from the caller.
//!
//! Mirrors mistlib-wasm's design one-for-one:
//! - `published_tracks`/`published_senders` bookkeeping mirrors wasm's
//!   `local_tracks`/`peer_senders`.
//! - `attach_published_tracks_to_peer` mirrors wasm's function of the same
//!   name.
//! - `publish_local_track`/`unpublish_local_track` mirror wasm's functions of
//!   the same name, including propagating the first renegotiation failure
//!   (rather than best-effort continuing past it) to keep behavior identical
//!   across platforms.

use std::sync::Arc;

use mistlib_core::types::NodeId;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::TrackLocal;

use super::{Peer, WebRtcTransport};

impl WebRtcTransport {
    /// `true` if at least one track is currently published. Used by
    /// `signaling::handle_offer` to decide whether a brand-new peer needs a
    /// follow-up renegotiation after its first answer (see the comment
    /// there for why that step can't be folded into the answer itself).
    pub(crate) fn has_published_tracks(&self) -> bool {
        !self.published_tracks.read().unwrap().is_empty()
    }

    /// Attaches every currently-published track that isn't already attached
    /// to `peer`'s `RTCPeerConnection`. Returns `true` if at least one track
    /// was newly attached, so the caller can decide whether (and how) to
    /// renegotiate. Called both from `connection::create_pc` (the new-peer
    /// hook -- attach before the peer's first offer/answer, mirroring
    /// mistlib-wasm's `create_pc`) and from `publish_local_track` (for peers
    /// that are already connected).
    pub(crate) async fn attach_published_tracks_to_peer(
        &self,
        remote_id: &NodeId,
        peer: &Arc<Peer>,
    ) -> crate::error::Result<bool> {
        let tracks: Vec<(String, Arc<TrackLocalStaticRTP>)> = {
            let lock = self.published_tracks.read().unwrap();
            lock.iter()
                .map(|(id, track)| (id.clone(), track.clone()))
                .collect()
        };

        if tracks.is_empty() {
            return Ok(false);
        }

        let mut changed = false;
        let mut senders = self.published_senders.write().await;
        let peer_senders = senders.entry(remote_id.clone()).or_default();

        for (track_id, track) in tracks {
            if peer_senders.contains_key(&track_id) {
                continue;
            }
            let sender = peer
                .add_local_track(track as Arc<dyn TrackLocal + Send + Sync>)
                .await?;
            peer_senders.insert(track_id, sender);
            changed = true;
        }

        Ok(changed)
    }

    /// Publishes `track` into this transport's room: every peer already
    /// connected gets it attached and its connection renegotiated (a fresh
    /// offer), and every peer that connects afterward gets it automatically
    /// at connection setup (`connection::create_pc`'s new-peer hook) -- no
    /// further action needed as new peers join. This is the building block
    /// behind cascade/SFU-style distribution: a native app re-publishing a
    /// track it received from one peer (e.g. a VRChat screen share received
    /// over tc-chat) so every other peer in the room gets it too, without
    /// each of them needing a direct connection to the original source.
    ///
    /// Publishing the same track id again just replaces the stored track
    /// (e.g. swapping in a new encoder instance) without re-attaching to
    /// peers that already have a sender for that id.
    pub async fn publish_local_track(
        &self,
        track: Arc<TrackLocalStaticRTP>,
    ) -> crate::error::Result<()> {
        let track_id = track.id().to_string();
        {
            let mut lock = self.published_tracks.write().unwrap();
            lock.insert(track_id, track);
        }

        let peers: Vec<(NodeId, Arc<Peer>)> = {
            let peers = self.peers.read().await;
            peers
                .iter()
                .map(|(id, p)| (id.clone(), p.clone()))
                .collect()
        };

        for (node_id, peer) in peers {
            let changed = self
                .attach_published_tracks_to_peer(&node_id, &peer)
                .await?;
            if changed {
                self.send_offer(&node_id, &peer)
                    .await
                    .map_err(crate::error::MistError::Core)?;
            }
        }

        Ok(())
    }

    /// Reverses `publish_local_track`: removes `track` from the published
    /// set (so peers connecting from this point on no longer receive it),
    /// then removes the corresponding sender from every peer that has one
    /// and renegotiates.
    pub async fn unpublish_local_track(
        &self,
        track: Arc<TrackLocalStaticRTP>,
    ) -> crate::error::Result<()> {
        let track_id = track.id().to_string();
        {
            let mut lock = self.published_tracks.write().unwrap();
            lock.remove(&track_id);
        }

        let removed_senders: Vec<(NodeId, Arc<RTCRtpSender>)> = {
            let mut senders = self.published_senders.write().await;
            senders
                .iter_mut()
                .filter_map(|(node_id, peer_senders)| {
                    peer_senders
                        .remove(&track_id)
                        .map(|sender| (node_id.clone(), sender))
                })
                .collect()
        };

        if removed_senders.is_empty() {
            return Ok(());
        }

        let peers = self.peers.read().await.clone();

        for (node_id, sender) in removed_senders {
            let Some(peer) = peers.get(&node_id) else {
                continue;
            };
            peer.pc.remove_track(&sender).await?;
            self.send_offer(&node_id, peer)
                .await
                .map_err(crate::error::MistError::Core)?;
        }

        Ok(())
    }
}
