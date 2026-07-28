use super::WasmNostrSignaler;
use gloo_timers::future::TimeoutFuture;
use rand::Rng;

const MIN_DISCOVERY_REFRESH_DELAY_MS: u64 = 500;
/// Matches `mistlib-native`'s discovery refresh jitter fraction, so
/// simultaneous peers with the same `ttl_seconds` don't all re-announce in
/// lockstep.
const JITTER_FRACTION: f64 = 0.25;

impl WasmNostrSignaler {
    pub(super) fn spawn_discovery_refresh(&self, room_id: String) {
        let expected_epoch = self.next_refresh_epoch();
        let signaler = self.clone();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                TimeoutFuture::new(discovery_refresh_delay_ms(
                    signaler.codec_config.ttl_seconds,
                ))
                .await;

                if !signaler.refresh_epoch_matches(expected_epoch) {
                    break;
                }
                if !signaler.room_is_current(&room_id) {
                    break;
                }
                if let Err(err) = signaler.publish_discovery(&room_id) {
                    web_sys::console::warn_1(
                        &format!(
                            "WasmNostrSignaler: discovery refresh failed for room {room_id}: {err:?}"
                        )
                        .into(),
                    );
                }
            }
        });
    }

    pub(super) fn cancel_discovery_refresh(&self) {
        let _ = self.next_refresh_epoch();
    }

    fn next_refresh_epoch(&self) -> u64 {
        let mut epoch = self.refresh_epoch.lock().unwrap_or_else(|e| e.into_inner());
        *epoch = epoch.saturating_add(1);
        *epoch
    }

    fn refresh_epoch_matches(&self, expected_epoch: u64) -> bool {
        *self.refresh_epoch.lock().unwrap_or_else(|e| e.into_inner()) == expected_epoch
    }
}

fn base_discovery_refresh_delay_ms(ttl_seconds: u64) -> u64 {
    ttl_seconds
        .saturating_mul(500)
        .max(MIN_DISCOVERY_REFRESH_DELAY_MS)
}

/// Pure +/-`JITTER_FRACTION` jitter calculation, taking `jitter` explicitly
/// so it is unit-testable without depending on randomness (mirrors
/// `mistlib-native`'s `reconnect_backoff_delay`/`discovery_refresh_delay`
/// split of a deterministic core function from its `rand`-backed caller).
pub(super) fn jittered_discovery_refresh_delay_ms(ttl_seconds: u64, jitter: f64) -> u32 {
    let base = base_discovery_refresh_delay_ms(ttl_seconds);
    let jitter = jitter.clamp(-JITTER_FRACTION, JITTER_FRACTION);
    let millis = (base as f64 * (1.0 + jitter)).round().max(0.0) as u64;
    millis
        .max(MIN_DISCOVERY_REFRESH_DELAY_MS)
        .min(u64::from(u32::MAX)) as u32
}

fn discovery_refresh_delay_ms(ttl_seconds: u64) -> u32 {
    let jitter = rand::thread_rng().gen_range(-JITTER_FRACTION..=JITTER_FRACTION);
    jittered_discovery_refresh_delay_ms(ttl_seconds, jitter)
}
