use super::WasmNostrSignaler;
use gloo_timers::future::TimeoutFuture;
use mistlib_core::signaling::nostr::DiscoveryPriority;
use web_time::{Duration, Instant};

impl WasmNostrSignaler {
    pub(super) fn queue_request(&self, pubkey: &str, room: &str, priority: DiscoveryPriority) {
        use rand::Rng;
        let delay = Duration::from_millis(rand::thread_rng().gen_range(100..=900));
        let epoch = *self.session_epoch.lock().unwrap_or_else(|e| e.into_inner());
        let identity = self.current_identity().public_key;
        let mut outbox = self.outbox.lock().unwrap_or_else(|e| e.into_inner());
        let evidence = self
            .bootstrap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .evidence(pubkey);
        let mut exchanges = self.exchanges.lock().unwrap_or_else(|e| e.into_inner());
        exchanges.observe_evidence(pubkey, evidence);
        exchanges.queued(pubkey);
        if let Some(dropped) = outbox.push(pubkey.to_owned(), priority, delay, Instant::now()) {
            tracing::info!(
                "Nostr pacing overflow drop peer={}",
                dropped.chars().take(8).collect::<String>()
            );
            exchanges.release(&dropped);
        }
        drop(exchanges);
        if outbox.worker_epoch.is_some() {
            return;
        }
        outbox.worker_epoch = Some(epoch);
        drop(outbox);
        let signaler = self.clone();
        let room = room.to_owned();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                if *signaler
                    .session_epoch
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    != epoch
                {
                    return;
                }
                let wait = {
                    let mut outbox = signaler.outbox.lock().unwrap_or_else(|e| e.into_inner());
                    match outbox.delay(Instant::now()) {
                        Some(wait) => wait,
                        None => {
                            outbox.worker_epoch = None;
                            return;
                        }
                    }
                };
                TimeoutFuture::new(wait.as_millis().min(u32::MAX as u128) as u32).await;
                if *signaler
                    .session_epoch
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    != epoch
                    || signaler.room_id().as_deref() != Some(&room)
                    || signaler.current_identity().public_key != identity
                {
                    return;
                }
                let item = signaler
                    .outbox
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pop(Instant::now());
                let Some(item) = item else {
                    continue;
                };
                if item.priority == DiscoveryPriority::Probe
                    && *signaler
                        .connected_peers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        >= mistlib_core::signaling::nostr::TARGET_PEERS
                {
                    signaler
                        .exchanges
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .release(&item.pubkey);
                    continue;
                }
                if !signaler
                    .exchanges
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending(&item.pubkey)
                {
                    continue;
                }
                match signaler.publish_reserved_request(&item.pubkey, &room) {
                    Ok(()) => signaler
                        .exchanges
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .log_sent(&item.pubkey),
                    Err(err) => tracing::warn!("Nostr paced discovery request failed: {err:?}"),
                }
                signaler
                    .outbox
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sent(Instant::now());
                signaler
                    .exchanges
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sent(&item.pubkey, Instant::now());
            }
        });
    }
}
