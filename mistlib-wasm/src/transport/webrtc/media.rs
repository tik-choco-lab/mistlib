use super::{LocalTrack, Peer, WasmWebRtcTransport};
use mistlib_core::types::NodeId;
use std::sync::Arc;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::{MediaStream, MediaStreamTrack, RtcRtpSender};

impl WasmWebRtcTransport {
    pub(super) fn attach_published_tracks_to_peer(
        &self,
        remote_id: &NodeId,
        peer: &Arc<Peer>,
    ) -> Result<bool, JsValue> {
        let tracks: Vec<(String, MediaStreamTrack)> = {
            let lock = self.local_tracks.read().unwrap_or_else(|e| e.into_inner());
            lock.iter()
                .filter(|(_, local)| local.published)
                .map(|(track_id, local)| (track_id.clone(), local.track.clone()))
                .collect()
        };

        let mut changed = false;
        let mut senders_lock = self.peer_senders.write().unwrap_or_else(|e| e.into_inner());
        let peer_senders = senders_lock.entry(remote_id.clone()).or_default();

        for (track_id, track) in tracks {
            if peer_senders.contains_key(&track_id) {
                continue;
            }
            let stream = MediaStream::new()?;
            stream.add_track(&track);
            let sender = peer.pc.add_track_0(&track, &stream);
            peer_senders.insert(track_id, sender);
            changed = true;
        }

        Ok(changed)
    }

    fn replace_track_for_existing_senders(&self, track_id: &str, track: &MediaStreamTrack) {
        let replacements: Vec<(NodeId, RtcRtpSender)> = self
            .peer_senders
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(node_id, senders)| {
                senders
                    .get(track_id)
                    .cloned()
                    .map(|sender| (node_id.clone(), sender))
            })
            .collect();

        for (node_id, sender) in replacements {
            let track = track.clone();
            let track_id = track_id.to_string();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(err) = JsFuture::from(sender.replace_track(Some(&track))).await {
                    tracing::error!(
                        "Failed to replace local track {} for peer {}: {:?}",
                        track_id,
                        node_id.0,
                        err
                    );
                }
            });
        }
    }

    pub fn register_local_track(
        &self,
        track_id: String,
        track: MediaStreamTrack,
    ) -> mistlib_core::error::Result<()> {
        let kind = track.kind();
        let published = {
            let mut lock = self.local_tracks.write().unwrap_or_else(|e| e.into_inner());
            let published = lock
                .get(&track_id)
                .map(|entry| entry.published)
                .unwrap_or(false);
            lock.insert(
                track_id.clone(),
                LocalTrack {
                    track: track.clone(),
                    kind,
                    published,
                },
            );
            published
        };

        if published {
            self.replace_track_for_existing_senders(&track_id, &track);
        }

        Ok(())
    }

    pub async fn publish_local_track(&self, track_id: &str) -> mistlib_core::error::Result<()> {
        {
            let mut lock = self.local_tracks.write().unwrap_or_else(|e| e.into_inner());
            let entry = lock.get_mut(track_id).ok_or_else(|| {
                mistlib_core::error::MistError::Internal(format!(
                    "Unknown local track: {}",
                    track_id
                ))
            })?;
            entry.published = true;
        }

        let peers: Vec<(NodeId, Arc<Peer>)> = self
            .peers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(node_id, peer)| (node_id.clone(), peer.clone()))
            .collect();

        for (node_id, peer) in peers {
            let changed = self
                .attach_published_tracks_to_peer(&node_id, &peer)
                .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;
            if changed {
                // A renegotiation rejection here is almost always a transient
                // peer state (ICE-disconnected recovery grace, a non-Stable
                // signaling moment) -- the senders were attached above, so
                // don't fail the publish over it: mark the peer for
                // reconciliation and let the ICE-Connected hook
                // (`reconcile_peer_tracks`) renegotiate once it recovers. A
                // peer that never recovers is torn down by the sweeper and
                // re-handshakes as a fresh `Peer`, which attaches published
                // tracks in `create_pc` before its first negotiation instead.
                if let Err(err) = self.renegotiate_peer(&node_id, &peer).await {
                    peer.needs_track_reconcile
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    tracing::info!(
                        "Deferred publishing local track {} to {} ({}); will renegotiate on recovery",
                        track_id,
                        node_id.0,
                        err
                    );
                }
            }
        }

        Ok(())
    }

    pub async fn unpublish_local_track(&self, track_id: &str) -> mistlib_core::error::Result<()> {
        {
            let mut lock = self.local_tracks.write().unwrap_or_else(|e| e.into_inner());
            let entry = lock.get_mut(track_id).ok_or_else(|| {
                mistlib_core::error::MistError::Internal(format!(
                    "Unknown local track: {}",
                    track_id
                ))
            })?;
            entry.published = false;
        }

        let peers: Vec<(NodeId, Arc<Peer>)> = self
            .peers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(node_id, peer)| (node_id.clone(), peer.clone()))
            .collect();

        for (node_id, peer) in peers {
            let sender = {
                let mut senders = self.peer_senders.write().unwrap_or_else(|e| e.into_inner());
                senders
                    .get_mut(&node_id)
                    .and_then(|peer_senders| peer_senders.remove(track_id))
            };

            if let Some(sender) = sender {
                peer.pc.remove_track(&sender);
                // Same deferral as `publish_local_track`: the sender is
                // already removed, so a transiently-rejected renegotiation
                // just means the remote keeps a dead m-line until the peer
                // recovers and `reconcile_peer_tracks` renegotiates.
                if let Err(err) = self.renegotiate_peer(&node_id, &peer).await {
                    peer.needs_track_reconcile
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    tracing::info!(
                        "Deferred unpublishing local track {} from {} ({}); will renegotiate on recovery",
                        track_id,
                        node_id.0,
                        err
                    );
                }
            }
        }

        Ok(())
    }

    /// `true` if at least one local track is currently published. Used by
    /// `handle_offer` to decide whether a brand-new peer we just *answered*
    /// needs a follow-up offer of our own -- mirrors `mistlib-native`'s
    /// `has_published_tracks` (see `signaling::handle_offer` there for the
    /// JSEP reasoning: an answer cannot introduce m-lines beyond the remote's
    /// offer, so tracks attached in `create_pc` aren't negotiated by the
    /// answer alone).
    pub(super) fn has_published_tracks(&self) -> bool {
        self.local_tracks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|track| track.published)
    }

    /// Recovery-path counterpart to the per-peer renegotiation in
    /// `publish_local_track`/`unpublish_local_track`: re-attaches any
    /// published track this peer is missing a sender for, then renegotiates
    /// unconditionally -- the caller (the ICE-Connected arm in
    /// `Peer::setup_handlers`, via `Peer::needs_track_reconcile`) only fires
    /// this when a track change already happened without its renegotiation,
    /// so "no newly attached track" does not mean "nothing to renegotiate".
    /// On failure the flag is re-set so the next recovery event retries,
    /// rather than losing the track change after all.
    pub(crate) async fn reconcile_peer_tracks(&self, node: &NodeId) {
        let peer = {
            let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
            peers.get(node).cloned()
        };
        // Peer already swept/replaced: nothing to reconcile -- a fresh peer
        // gets published tracks attached in `create_pc` pre-negotiation.
        let Some(peer) = peer else { return };

        if let Err(err) = self.attach_published_tracks_to_peer(node, &peer) {
            tracing::warn!(
                "Failed to re-attach published tracks to {} during reconcile: {:?}",
                node.0,
                err
            );
        }

        match self.renegotiate_peer(node, &peer).await {
            Ok(()) => {
                tracing::info!(
                    "Reconciled deferred track changes with {} after recovery",
                    node.0
                );
            }
            Err(err) => {
                peer.needs_track_reconcile
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                tracing::debug!(
                    "Track reconcile with {} deferred again ({}); will retry on next recovery",
                    node.0,
                    err
                );
            }
        }
    }

    pub async fn remove_local_track(&self, track_id: &str) -> mistlib_core::error::Result<()> {
        let _ = self.unpublish_local_track(track_id).await;
        let mut lock = self.local_tracks.write().unwrap_or_else(|e| e.into_inner());
        lock.remove(track_id);
        Ok(())
    }

    pub fn set_local_track_enabled(
        &self,
        track_id: &str,
        enabled: bool,
    ) -> mistlib_core::error::Result<()> {
        let local_track = {
            let lock = self.local_tracks.read().unwrap_or_else(|e| e.into_inner());
            let entry = lock.get(track_id).ok_or_else(|| {
                mistlib_core::error::MistError::Internal(format!(
                    "Unknown local track: {}",
                    track_id
                ))
            })?;
            entry.track.clone()
        };
        local_track.set_enabled(enabled);

        let sender_tracks: Vec<MediaStreamTrack> = self
            .peer_senders
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(|senders| senders.get(track_id))
            .filter_map(|sender| sender.track())
            .collect();

        for sender_track in sender_tracks {
            sender_track.set_enabled(enabled);
        }

        Ok(())
    }

    pub fn get_local_track(&self, track_id: &str) -> Option<MediaStreamTrack> {
        self.local_tracks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(track_id)
            .map(|entry| entry.track.clone())
    }
}
