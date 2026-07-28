use mistlib_core::signaling::nostr::{build_discovery_event_with_joined_at, event_frame_json};
use mistlib_core::stats::STATS;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use super::NostrSignaler;

const MIN_DISCOVERY_REFRESH_DELAY: Duration = Duration::from_millis(500);

impl NostrSignaler {
    pub(super) fn spawn_discovery_refresh(&self, room_id: String) {
        let expected_epoch = self
            .refresh_epoch
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        let refresh_epoch = self.refresh_epoch.clone();
        let room_state = Arc::downgrade(&self.room_id);
        let senders = Arc::downgrade(&self.senders);
        let codec_config = self.codec_config.clone();
        let crypto = self.crypto.clone();
        let identity = self.identity.clone();
        let rotated_identity = Arc::downgrade(&self.rotated_identity);
        let local_joined_at = Arc::downgrade(&self.local_joined_at);
        let ttl_seconds = self.codec_config.ttl_seconds;

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(discovery_refresh_delay(ttl_seconds)).await;

                if refresh_epoch.load(Ordering::SeqCst) != expected_epoch {
                    break;
                }

                let Some(room_state) = room_state.upgrade() else {
                    break;
                };
                if room_state.lock().await.as_deref() != Some(room_id.as_str()) {
                    break;
                }

                let Some(rotated_identity) = rotated_identity.upgrade() else {
                    break;
                };
                let active_identity = rotated_identity
                    .lock()
                    .await
                    .clone()
                    .unwrap_or_else(|| identity.clone());
                let joined_at = match local_joined_at.upgrade() {
                    Some(joined_at) => *joined_at.lock().await,
                    None => break,
                };
                let frame = match build_discovery_event_with_joined_at(
                    &codec_config,
                    &crypto,
                    &active_identity,
                    &room_id,
                    joined_at,
                )
                .and_then(|event| event_frame_json(&event))
                {
                    Ok(frame) => frame,
                    Err(err) => {
                        tracing::warn!(
                            "NostrSignaler: discovery refresh failed for room {}: {:?}",
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
                        "NostrSignaler: discovery refresh skipped for room {}: no relay connection is open",
                        room_id
                    );
                    continue;
                }
                for tx in relay_senders {
                    if let Err(err) = tx.send(frame.clone()).await {
                        tracing::warn!(
                            "NostrSignaler: discovery refresh send failed for room {}: {}",
                            room_id,
                            err
                        );
                    }
                }
                STATS.add_send(frame.len() as u64);
            }
        });
    }

    pub(super) fn cancel_discovery_refresh(&self) {
        self.refresh_epoch.fetch_add(1, Ordering::SeqCst);
    }
}

fn base_discovery_refresh_delay(ttl_seconds: u64) -> Duration {
    Duration::from_millis(ttl_seconds.saturating_mul(500)).max(MIN_DISCOVERY_REFRESH_DELAY)
}

#[cfg(test)]
fn discovery_refresh_delay(ttl_seconds: u64) -> Duration {
    base_discovery_refresh_delay(ttl_seconds)
}

#[cfg(not(test))]
fn discovery_refresh_delay(ttl_seconds: u64) -> Duration {
    use rand::{rngs::OsRng, Rng};

    let base = base_discovery_refresh_delay(ttl_seconds);
    let base_ms = base.as_millis().min(u128::from(u64::MAX)) as u64;
    let min_ms = base_ms.saturating_mul(3) / 4;
    let max_ms = base_ms.saturating_mul(5) / 4;
    Duration::from_millis(OsRng.gen_range(min_ms..=max_ms).max(500))
}
