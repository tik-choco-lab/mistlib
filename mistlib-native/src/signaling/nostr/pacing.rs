use super::NostrSignaler;
use mistlib_core::signaling::nostr::DiscoveryPriority;
use web_time::{Duration, Instant};

impl NostrSignaler {
    pub(super) async fn queue_request(
        &self,
        pubkey: &str,
        room: &str,
        epoch: u64,
        priority: DiscoveryPriority,
        delay: Duration,
    ) {
        let identity = self.current_identity().await.public_key;
        let mut outbox = self.outbox.lock().await;
        if !self.session_is_current(epoch) {
            return;
        }
        let evidence = self.bootstrap.lock().await.evidence(pubkey);
        let mut exchanges = self.exchanges.lock().await;
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
        let cancel = self.pacing_cancel.lock().await.clone();
        let signaler = self.clone();
        let room = room.to_owned();
        let worker = async move {
            loop {
                if !signaler.session_is_current(epoch) {
                    return;
                }
                let wait = {
                    let mut outbox = signaler.outbox.lock().await;
                    if outbox.worker_epoch != Some(epoch) {
                        return;
                    }
                    match outbox.delay(Instant::now()) {
                        Some(wait) => wait,
                        None => {
                            outbox.worker_epoch = None;
                            return;
                        }
                    }
                };
                tokio::time::sleep(wait).await;
                if !signaler.session_is_current(epoch)
                    || signaler.current_room_id().await.as_deref() != Some(&room)
                    || signaler.current_identity().await.public_key != identity
                {
                    return;
                }
                let item = {
                    let mut outbox = signaler.outbox.lock().await;
                    if outbox.worker_epoch != Some(epoch) {
                        return;
                    }
                    outbox.pop(Instant::now())
                };
                let Some(item) = item else {
                    continue;
                };
                if !signaler.exchanges.lock().await.pending(&item.pubkey) {
                    continue;
                }
                let result = signaler
                    .publish_reserved_request(
                        &item.pubkey,
                        &room,
                        epoch,
                        item.priority == DiscoveryPriority::Probe,
                    )
                    .await;
                match result {
                    Ok(true) => signaler.exchanges.lock().await.log_sent(&item.pubkey),
                    Ok(false) => {
                        if signaler.session_is_current(epoch) {
                            signaler.exchanges.lock().await.release(&item.pubkey);
                        }
                        continue;
                    }
                    Err(err) => tracing::warn!("Nostr paced discovery request failed: {err:?}"),
                }
                if signaler.session_is_current(epoch) {
                    signaler.outbox.lock().await.sent(Instant::now());
                    signaler
                        .exchanges
                        .lock()
                        .await
                        .sent(&item.pubkey, Instant::now());
                }
            }
        };
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = worker => {}
            }
        });
    }
}
