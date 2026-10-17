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
//!   the same name, including the tolerant renegotiation: a rejected offer is
//!   deferred (`Peer::needs_track_reconcile`, retried by the sweeper) instead
//!   of failing the call.
//! - `set_local_track_audience` mirrors wasm's function of the same name: a
//!   track is only attached to peers in its audience (`None` = every peer,
//!   `Some([])` = nobody), whether it is published before or after the
//!   audience is set, and for peers that connect later.

use std::collections::HashMap;
use std::sync::Arc;

use mistlib_core::transport::track_audience::TrackAudience;
use mistlib_core::types::NodeId;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::TrackLocal;

use super::{Peer, WebRtcTransport};

type ChangedPeers = Vec<(NodeId, Arc<Peer>)>;

impl WebRtcTransport {
    /// `true` if at least one track is currently published to `remote_id`
    /// (published and the peer is in its audience). Used by
    /// `signaling::handle_offer` to decide whether a brand-new peer needs a
    /// follow-up renegotiation after its first answer (see the comment
    /// there for why that step can't be folded into the answer itself).
    pub(crate) fn has_published_tracks_for(&self, remote_id: &NodeId) -> bool {
        let published = self.published_tracks.read().unwrap();
        let registered = self.registered_tracks.read().unwrap();
        published
            .keys()
            .any(|id| Self::audience_allows(&registered, id, remote_id))
    }

    fn audience_allows(
        registered: &HashMap<String, TrackAudience>,
        track_id: &str,
        remote_id: &NodeId,
    ) -> bool {
        registered
            .get(track_id)
            .is_none_or(|audience| audience.allows(remote_id))
    }

    /// Brings `peer`'s senders in line with the published tracks it is in the
    /// audience of: attaches the missing ones, swaps a replaced track and
    /// removes senders it should no longer have. Returns `true` if a sender
    /// was added or removed, so the caller can renegotiate. Called from
    /// `connection::create_pc` (the new-peer hook -- attach before the peer's
    /// first offer/answer, mirroring mistlib-wasm's `create_pc`).
    pub(crate) async fn attach_published_tracks_to_peer(
        &self,
        remote_id: &NodeId,
        peer: &Arc<Peer>,
    ) -> crate::error::Result<bool> {
        let _changes = self.media_changes.lock().await;
        self.sync_peer_tracks(remote_id, peer).await
    }

    /// Re-syncs one just-registered peer against the *current* audience.
    /// `create_pc` attaches tracks before the peer is visible in `peers`, so
    /// an audience change in that window (which iterates `peers`) would miss
    /// it. Callers run this right after inserting the peer and before the
    /// first offer/answer, so any attach or detach rides on that negotiation;
    /// no extra renegotiation is needed.
    pub(crate) async fn resync_registered_peer(&self, remote_id: &NodeId, peer: &Arc<Peer>) {
        let _changes = self.media_changes.lock().await;
        if let Err(err) = self.sync_peer_tracks(remote_id, peer).await {
            tracing::info!("Deferred media sync with new peer {}: {}", remote_id, err);
            peer.needs_track_reconcile
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    async fn sync_peer_tracks(
        &self,
        remote_id: &NodeId,
        peer: &Arc<Peer>,
    ) -> crate::error::Result<bool> {
        let tracks: Vec<(String, Arc<TrackLocalStaticRTP>)> = {
            let published = self.published_tracks.read().unwrap();
            let registered = self.registered_tracks.read().unwrap();
            published
                .iter()
                .filter(|(id, _)| Self::audience_allows(&registered, id, remote_id))
                .map(|(id, track)| (id.clone(), track.clone()))
                .collect()
        };

        let mut changed = false;
        let mut senders = self.published_senders.write().await;
        if tracks.is_empty() && !senders.contains_key(remote_id) {
            return Ok(false);
        }
        let peer_senders = senders.entry(remote_id.clone()).or_default();

        let removed: Vec<String> = peer_senders
            .keys()
            .filter(|id| !tracks.iter().any(|(track_id, _)| track_id == *id))
            .cloned()
            .collect();
        for id in removed {
            if let Some(sender) = peer_senders.get(&id) {
                peer.pc.remove_track(sender).await?;
            }
            peer_senders.remove(&id);
            changed = true;
        }
        for (track_id, track) in tracks {
            let track = track as Arc<dyn TrackLocal + Send + Sync>;
            if let Some(sender) = peer_senders.get(&track_id) {
                let same = sender
                    .track()
                    .await
                    .is_some_and(|old| std::ptr::addr_eq(Arc::as_ptr(&old), Arc::as_ptr(&track)));
                if !same {
                    sender.replace_track(Some(track)).await?;
                }
                continue;
            }
            let sender = peer.add_local_track(track).await?;
            peer_senders.insert(track_id, sender);
            changed = true;
        }

        Ok(changed)
    }

    /// Syncs every connected peer; returns those whose senders changed (or
    /// failed to sync). A failing peer is deferred, not allowed to fail the
    /// whole call.
    async fn sync_all_peers(&self) -> ChangedPeers {
        let peers = self.peers.read().await.clone();
        let mut changed = Vec::new();
        for (node, peer) in peers {
            match self.sync_peer_tracks(&node, &peer).await {
                Ok(false) => {}
                Ok(true) => changed.push((node, peer)),
                Err(err) => {
                    tracing::info!("Deferred media sync with {}: {}", node, err);
                    changed.push((node, peer));
                }
            }
        }
        changed
    }

    /// Renegotiates the peers `sync_all_peers` reported. Runs after
    /// `media_changes` is released; a rejected offer stays flagged on the peer
    /// and is retried (`PeerSharedHandles::reconcile_track_offer`, sweeper).
    async fn reconcile_changed_peers(&self, changed: ChangedPeers) {
        for (node, peer) in changed {
            peer.needs_track_reconcile
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.peer_handles()
                .reconcile_track_offer(&node, &peer)
                .await;
        }
    }

    fn register_track(&self, track: &TrackLocalStaticRTP) {
        self.registered_tracks
            .write()
            .unwrap()
            .entry(track.id().to_string())
            .or_default();
    }

    /// Registers or replaces a track without publishing it, keeping its
    /// audience and publication state. Lets the audience be set before the
    /// first publish.
    pub async fn register_local_track(
        &self,
        track: Arc<TrackLocalStaticRTP>,
    ) -> crate::error::Result<()> {
        let changed = {
            let _changes = self.media_changes.lock().await;
            let id = track.id().to_string();
            self.register_track(&track);
            if let Some(entry) = self.published_tracks.write().unwrap().get_mut(&id) {
                *entry = track;
            }
            self.sync_all_peers().await
        };
        self.reconcile_changed_peers(changed).await;
        Ok(())
    }

    /// Limits `track_id` to `nodes`: `None` = every peer (the default),
    /// `Some([])` = nobody. Works on a registered track whether or not it is
    /// published; a published track is detached from peers that left the
    /// audience and attached to peers that joined it, with renegotiation.
    pub async fn set_local_track_audience(
        &self,
        track_id: &str,
        nodes: Option<Vec<NodeId>>,
    ) -> crate::error::Result<()> {
        let changed = {
            let _changes = self.media_changes.lock().await;
            {
                let mut tracks = self.registered_tracks.write().unwrap();
                let audience = tracks.get_mut(track_id).ok_or_else(|| {
                    crate::error::MistError::Internal(format!("Unknown local track: {}", track_id))
                })?;
                *audience = TrackAudience::new(nodes);
            }
            self.sync_all_peers().await
        };
        self.reconcile_changed_peers(changed).await;
        Ok(())
    }

    /// Unpublishes and forgets `track_id`; its audience is cleared.
    pub async fn remove_local_track(&self, track_id: &str) -> crate::error::Result<()> {
        let changed = {
            let _changes = self.media_changes.lock().await;
            let removed = self.registered_tracks.write().unwrap().remove(track_id);
            if removed.is_none() {
                return Err(crate::error::MistError::Internal(format!(
                    "Unknown local track: {}",
                    track_id
                )));
            }
            self.published_tracks.write().unwrap().remove(track_id);
            self.sync_all_peers().await
        };
        self.reconcile_changed_peers(changed).await;
        Ok(())
    }

    /// Publishes `track` into this transport's room: every peer already
    /// connected (and in the track's audience, if one was set) gets it
    /// attached and its connection renegotiated (a fresh offer), and every
    /// peer that connects afterward gets it automatically at connection setup
    /// (`connection::create_pc`'s new-peer hook) -- no further action needed
    /// as new peers join. This is the building block behind cascade/SFU-style
    /// distribution: a native app re-publishing a track it received from one
    /// peer (e.g. a VRChat screen share received over tc-chat) so every other
    /// peer in the room gets it too, without each of them needing a direct
    /// connection to the original source.
    ///
    /// Publishing the same track id again just replaces the stored track
    /// (e.g. swapping in a new encoder instance) without re-attaching to
    /// peers that already have a sender for that id.
    pub async fn publish_local_track(
        &self,
        track: Arc<TrackLocalStaticRTP>,
    ) -> crate::error::Result<()> {
        let changed = {
            let _changes = self.media_changes.lock().await;
            self.register_track(&track);
            let track_id = track.id().to_string();
            self.published_tracks
                .write()
                .unwrap()
                .insert(track_id, track);
            self.sync_all_peers().await
        };
        self.reconcile_changed_peers(changed).await;
        Ok(())
    }

    /// Reverses `publish_local_track`: removes `track` from the published
    /// set (so peers connecting from this point on no longer receive it),
    /// then removes the corresponding sender from every peer that has one
    /// and renegotiates. The track stays registered (audience kept).
    pub async fn unpublish_local_track(
        &self,
        track: Arc<TrackLocalStaticRTP>,
    ) -> crate::error::Result<()> {
        let changed = {
            let _changes = self.media_changes.lock().await;
            self.published_tracks.write().unwrap().remove(track.id());
            self.sync_all_peers().await
        };
        self.reconcile_changed_peers(changed).await;
        Ok(())
    }
}
