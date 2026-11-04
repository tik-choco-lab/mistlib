use super::WasmNostrSignaler;
use gloo_timers::future::TimeoutFuture;

impl WasmNostrSignaler {
    /// Healthy sessions refresh room subscriptions. Isolated/suspended sessions
    /// send a short-lived limit:0 probe, avoiding historical discovery replay
    /// while still checking a relay round trip every thirty seconds.
    pub(super) fn spawn_relay_keepalive(&self, room_id: String) {
        let expected_epoch = self.next_keepalive_epoch();
        let signaler = self.clone();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                TimeoutFuture::new(super::RELAY_KEEPALIVE_INTERVAL_MS).await;

                if !signaler.keepalive_epoch_matches(expected_epoch) {
                    break;
                }
                if !signaler.room_is_current(&room_id) {
                    break;
                }
                let now = web_time::Instant::now();
                let isolated = *signaler.connected_peers.lock().unwrap() == 0;
                if isolated || signaler.governor.lock().unwrap().suspended(now) {
                    let frame = probe_frame(&signaler.codec_config, &room_id);
                    if let Ok(frame) = frame {
                        if let Err(err) = signaler.publish_frame(frame) {
                            tracing::info!("Nostr keepalive probe failed: {err}");
                        }
                    }
                    continue;
                }
                if let Err(err) = signaler.subscribe_room(&room_id) {
                    web_sys::console::warn_1(
                        &format!(
                            "WasmNostrSignaler: relay keepalive re-subscribe failed for room {room_id}: {err:?}"
                        )
                        .into(),
                    );
                }
            }
        });
    }

    /// Stops the active keepalive loop, if any. Called on room change (a
    /// fresh call to `spawn_relay_keepalive` also implicitly cancels the
    /// previous loop via the epoch bump) and on `close()`.
    pub(super) fn cancel_relay_keepalive(&self) {
        let _ = self.next_keepalive_epoch();
    }

    fn next_keepalive_epoch(&self) -> u64 {
        let mut epoch = self
            .keepalive_epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *epoch = epoch.saturating_add(1);
        *epoch
    }

    fn keepalive_epoch_matches(&self, expected_epoch: u64) -> bool {
        *self
            .keepalive_epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            == expected_epoch
    }
}

const PROBE_ID: &str = "mistlib-keepalive";

pub(super) fn probe_close_frame(raw: &str) -> Option<String> {
    use mistlib_core::signaling::nostr::{close_frame_json, parse_relay_message, RelayMessage};
    match parse_relay_message(raw).ok()?? {
        RelayMessage::Eose { subscription_id } if subscription_id == PROBE_ID => {
            close_frame_json(PROBE_ID).ok()
        }
        _ => None,
    }
}

pub(super) fn probe_frame(
    config: &mistlib_core::signaling::nostr::NostrCodecConfig,
    room: &str,
) -> mistlib_core::error::Result<String> {
    use mistlib_core::signaling::nostr::{discovery_filter, req_frame_json};
    let frame = req_frame_json(PROBE_ID, &[discovery_filter(config, room)])?;
    let mut value: serde_json::Value = serde_json::from_str(&frame)?;
    value[2]["limit"] = serde_json::json!(0);
    Ok(serde_json::to_string(&value)?)
}
