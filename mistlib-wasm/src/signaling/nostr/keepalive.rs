use super::WasmNostrSignaler;
use gloo_timers::future::TimeoutFuture;

impl WasmNostrSignaler {
    /// Periodically re-issues this instance's REQ subscriptions (stable
    /// `discovery_subscription_id`/`message_subscription_id`) on every open
    /// relay socket for `room_id`.
    ///
    /// Browser `WebSocket` exposes no ping API, so unlike `mistlib-native`
    /// (which pings every relay connection directly), wasm keeps relay
    /// connections alive by resending the REQ frame at
    /// `RELAY_KEEPALIVE_INTERVAL_MS`. Because the subscription id is
    /// unchanged, the relay treats this as a filter replace (NIP-01) rather
    /// than a new subscription, and responds with EOSE, which produces the
    /// round-trip traffic that both refreshes the accepted room-scope
    /// window (`discovery_filter`/`message_filter` recompute the scope on
    /// every call) and updates each socket's `last_activity` for the idle
    /// watchdog in `connection.rs`.
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
