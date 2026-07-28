use mistlib_core::signaling::nostr::{discovery_filter, message_filter, req_frame_json};
use mistlib_core::stats::STATS;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use super::{NostrSignaler, RoomSubscriptionIds};

/// Periodic room-scope resubscribe.
///
/// The room scope ("d" tag) rotates every
/// `NostrCodecConfig::room_scope_rotation_seconds()`. `discovery_filter`/
/// `message_filter` embed a static 3-bucket scope window computed at REQ
/// time, so a connection that stays open across a rotation boundary keeps an
/// increasingly stale filter and silently stops matching events from peers
/// using the new scope. Re-issuing the REQ with the room's persisted
/// subscription ids (see `RoomSubscriptionIds`) at least once per rotation
/// keeps the relay-side filter current without opening new subscriptions.
impl NostrSignaler {
    pub(super) fn spawn_room_resubscribe(&self, room_id: String) {
        let expected_epoch = self
            .resubscribe_epoch
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        let resubscribe_epoch = self.resubscribe_epoch.clone();
        let room_state = Arc::downgrade(&self.room_id);
        let senders = Arc::downgrade(&self.senders);
        let subscription_ids = Arc::downgrade(&self.subscription_ids);
        let identity_pubkey = self.identity.public_key.clone();
        let rotated_identity = Arc::downgrade(&self.rotated_identity);
        let codec_config = self.codec_config.clone();
        let rotation_seconds = codec_config.room_scope_rotation_seconds();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(room_resubscribe_delay(rotation_seconds)).await;

                if resubscribe_epoch.load(Ordering::SeqCst) != expected_epoch {
                    break;
                }

                let Some(room_state) = room_state.upgrade() else {
                    break;
                };
                if room_state.lock().await.as_deref() != Some(room_id.as_str()) {
                    break;
                }

                let Some(subscription_ids) = subscription_ids.upgrade() else {
                    break;
                };
                let ids = {
                    let mut lock = subscription_ids.lock().await;
                    match lock.as_ref() {
                        Some(ids) => ids.clone(),
                        None => {
                            let ids = RoomSubscriptionIds::generate();
                            *lock = Some(ids.clone());
                            ids
                        }
                    }
                };

                let Some(rotated_identity) = rotated_identity.upgrade() else {
                    break;
                };
                let local_pubkey = rotated_identity
                    .lock()
                    .await
                    .as_ref()
                    .map(|identity| identity.public_key.clone())
                    .unwrap_or_else(|| identity_pubkey.clone());

                let discovery = discovery_filter(&codec_config, &room_id);
                let message = message_filter(&codec_config, &room_id, &local_pubkey);
                let discovery_frame = match req_frame_json(&ids.discovery, &[discovery]) {
                    Ok(frame) => frame,
                    Err(err) => {
                        tracing::warn!(
                            "NostrSignaler: resubscribe filter encode failed for room {}: {:?}",
                            room_id,
                            err
                        );
                        continue;
                    }
                };
                let message_frame = match req_frame_json(&ids.message, &[message]) {
                    Ok(frame) => frame,
                    Err(err) => {
                        tracing::warn!(
                            "NostrSignaler: resubscribe filter encode failed for room {}: {:?}",
                            room_id,
                            err
                        );
                        continue;
                    }
                };

                let Some(senders) = senders.upgrade() else {
                    break;
                };
                let relay_senders = { senders.lock().await.clone() };
                if relay_senders.is_empty() {
                    tracing::warn!(
                        "NostrSignaler: room resubscribe skipped for room {}: no relay connection is open",
                        room_id
                    );
                    continue;
                }
                for tx in &relay_senders {
                    if let Err(err) = tx.send(discovery_frame.clone()).await {
                        tracing::warn!(
                            "NostrSignaler: room resubscribe send failed for room {}: {}",
                            room_id,
                            err
                        );
                    }
                }
                for tx in &relay_senders {
                    if let Err(err) = tx.send(message_frame.clone()).await {
                        tracing::warn!(
                            "NostrSignaler: room resubscribe send failed for room {}: {}",
                            room_id,
                            err
                        );
                    }
                }
                STATS.add_send((discovery_frame.len() + message_frame.len()) as u64);
            }
        });
    }

    pub(super) fn cancel_room_resubscribe(&self) {
        self.resubscribe_epoch.fetch_add(1, Ordering::SeqCst);
    }
}

/// Base interval between periodic resubscribes: half the room-scope
/// rotation period, so a connection re-issues its REQ at least once before
/// the previous scope bucket falls out of the accepted window.
#[cfg(not(test))]
fn base_room_resubscribe_delay(rotation_seconds: u64) -> Duration {
    Duration::from_millis(rotation_seconds.saturating_mul(500).max(1))
}

#[cfg(test)]
fn room_resubscribe_delay(_rotation_seconds: u64) -> Duration {
    // NostrCodecConfig::room_scope_rotation_seconds() floors at 30s
    // regardless of the test config's `ttl_seconds`, so tests use a fixed
    // fast interval instead of deriving one from rotation_seconds. Kept well
    // above the CLOSED-triggered resubscribe test's assertion window so the
    // two mechanisms stay distinguishable.
    Duration::from_millis(300)
}

#[cfg(not(test))]
fn room_resubscribe_delay(rotation_seconds: u64) -> Duration {
    use rand::{rngs::OsRng, Rng};

    let base = base_room_resubscribe_delay(rotation_seconds);
    let base_ms = base.as_millis().min(u128::from(u64::MAX)) as u64;
    let min_ms = base_ms.saturating_mul(3) / 4;
    let max_ms = base_ms.saturating_mul(5) / 4;
    Duration::from_millis(OsRng.gen_range(min_ms..=max_ms).max(1))
}
