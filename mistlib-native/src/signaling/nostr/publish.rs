use super::NostrSignaler;
use mistlib_core::signaling::nostr::DiscoveryPriority;
use mistlib_core::signaling::nostr::{
    build_discovery_event_with_joined_at, build_message_event_with_sequence_and_joined_at,
    event_frame_json, next_outgoing_sequence,
};
use mistlib_core::signaling::{SignalingData, SignalingType};
use mistlib_core::stats::STATS;
use mistlib_core::types::NodeId;
use tokio::time::Duration;

#[cfg(not(test))]
fn discovery_request_jitter() -> Duration {
    use rand::{rngs::OsRng, Rng};
    Duration::from_millis(OsRng.gen_range(100..=900))
}

impl NostrSignaler {
    pub(super) async fn publish_frame(&self, frame: String) -> mistlib_core::error::Result<()> {
        self.publish_frame_guarded(frame, None).await.map(|_| ())
    }

    async fn publish_frame_guarded(
        &self,
        frame: String,
        discovery: Option<(&str, u64, bool)>,
    ) -> mistlib_core::error::Result<bool> {
        let mut senders = self.senders.lock().await;
        if senders.is_empty() {
            return Err(mistlib_core::error::MistError::Signaling(
                "NostrSignaler: no relay connection is open".to_string(),
            ));
        }

        let mut delivered = 0_usize;
        let mut alive = Vec::with_capacity(senders.len());
        let mut backpressured = Vec::new();
        // Keep registered senders intact if an enqueue is cancelled.
        for tx in senders.iter() {
            if tx.is_closed() {
                tracing::warn!("NostrSignaler: dropping closed relay sender");
                continue;
            }
            // A stalled writer must not hold publication locks indefinitely.
            // Keep its sender for later retries, and still try healthy relays.
            let reserve = async {
                loop {
                    tokio::select! {
                        permit = tx.reserve() => return Some(permit),
                        _ = tokio::time::sleep(Duration::from_millis(25)), if discovery.is_some() => {
                            let (pubkey, epoch, proactive) = discovery.unwrap();
                            if !self.session_is_current(epoch)
                                || !self.exchanges.lock().await.pending(pubkey)
                                || (proactive && self.connected_peers.load(std::sync::atomic::Ordering::SeqCst)
                                    >= mistlib_core::signaling::nostr::TARGET_PEERS)
                            {
                                return None;
                            }
                        }
                    }
                }
            };
            let permit = match tokio::time::timeout(Duration::from_secs(1), reserve).await {
                Ok(Some(permit)) => permit,
                Ok(None) => {
                    if delivered > 0 {
                        STATS.add_send(frame.len() as u64);
                    }
                    return Ok(delivered > 0);
                }
                Err(_) => {
                    tracing::warn!("NostrSignaler: relay enqueue timed out");
                    alive.push(tx.clone());
                    backpressured.push(tx.clone());
                    continue;
                }
            };
            match permit {
                Ok(permit) => {
                    if let Some((pubkey, epoch, proactive)) = discovery {
                        // Backpressure can suspend even after send_order. Check
                        // progress once space exists, then enqueue without an
                        // await while the progress guard is held.
                        let exchanges = self.exchanges.lock().await;
                        if !self.session_is_current(epoch)
                            || !exchanges.pending(pubkey)
                            || (proactive
                                && self
                                    .connected_peers
                                    .load(std::sync::atomic::Ordering::SeqCst)
                                    >= mistlib_core::signaling::nostr::TARGET_PEERS)
                        {
                            if delivered > 0 {
                                STATS.add_send(frame.len() as u64);
                            }
                            return Ok(delivered > 0);
                        }
                        permit.send(frame.clone());
                    } else {
                        permit.send(frame.clone());
                    }
                    delivered += 1;
                    alive.push(tx.clone());
                }
                Err(err) => {
                    tracing::warn!("NostrSignaler: dropping dead relay sender: {}", err);
                }
            }
        }
        // Discovery may give up on backpressure, but a negotiation frame
        // (Answer/ACK) that no relay accepted must wait for capacity instead
        // of being dropped. Healthy relays were already served above.
        if delivered == 0 && discovery.is_none() {
            for tx in &backpressured {
                if let Ok(permit) = tx.reserve().await {
                    permit.send(frame.clone());
                    delivered += 1;
                    break;
                }
            }
        }
        *senders = alive;

        if delivered == 0 {
            return Err(mistlib_core::error::MistError::Signaling(
                "NostrSignaler: no relay accepted the frame".to_string(),
            ));
        }
        STATS.add_send(frame.len() as u64);
        Ok(true)
    }

    async fn publish_event(
        &self,
        event: &mistlib_core::signaling::nostr::NostrEvent,
    ) -> mistlib_core::error::Result<()> {
        let frame = event_frame_json(event)?;
        self.publish_frame(frame).await
    }

    pub(super) async fn publish_discovery(&self, room_id: &str) -> mistlib_core::error::Result<()> {
        let identity = self.current_identity().await;
        let joined_at = *self.local_joined_at.lock().await;
        let event = build_discovery_event_with_joined_at(
            &self.codec_config,
            &self.crypto,
            &identity,
            room_id,
            joined_at,
        )?;
        self.publish_event(&event).await
    }

    pub(super) async fn publish_message_to_pubkey(
        &self,
        receiver_pubkey: &str,
        data: &SignalingData,
    ) -> mistlib_core::error::Result<()> {
        self.publish_message_in_session(receiver_pubkey, data, self.session_epoch(), false, false)
            .await
            .map(|_| ())
    }

    async fn publish_message_in_session(
        &self,
        receiver_pubkey: &str,
        data: &SignalingData,
        expected_epoch: u64,
        proactive: bool,
        reserved: bool,
    ) -> mistlib_core::error::Result<bool> {
        // `Rejoin` is synthesized locally by this signaler for its own
        // transport to consume (see `SignalingType::Rejoin`'s doc comment)
        // and must never reach a relay -- even if every caller above this
        // point believes it is unreachable for `Rejoin`, this is the last
        // point before anything goes on the wire, so it guards
        // unconditionally.
        if data.signaling_type.is_local_only() {
            return Ok(false);
        }
        // Held from sequence assignment through the enqueue below so the two
        // steps happen atomically with respect to other targeted publishes;
        // see the `send_order` field doc comment on `NostrSignaler` for the
        // race this closes.
        let _send_order = self.send_order.lock().await;
        if !self.session_is_current(expected_epoch)
            || (reserved && !self.exchanges.lock().await.pending(receiver_pubkey))
            || (proactive
                && self
                    .connected_peers
                    .load(std::sync::atomic::Ordering::SeqCst)
                    >= mistlib_core::signaling::nostr::TARGET_PEERS)
        {
            return Ok(false);
        }
        let sequence = self.next_outgoing_sequence(receiver_pubkey).await;
        let identity = self.current_identity().await;
        // Carrying our own session epoch (`local_joined_at`, set on room
        // join/reset -- see `set_room_id`/`reset_session`) on every targeted
        // message, not just discovery announces, lets a peer detect a rejoin
        // (this identity's pubkey rotating under the same NodeId) even if it
        // misses our discovery re-announce and only ever sees our targeted
        // messages -- see `bind_node_with_epoch` on the receiving side.
        let joined_at = *self.local_joined_at.lock().await;
        let event = build_message_event_with_sequence_and_joined_at(
            &self.codec_config,
            &self.crypto,
            &identity,
            receiver_pubkey,
            data,
            sequence,
            joined_at,
        )?;
        if !self.session_is_current(expected_epoch)
            || (reserved && !self.exchanges.lock().await.pending(receiver_pubkey))
            || (proactive
                && self
                    .connected_peers
                    .load(std::sync::atomic::Ordering::SeqCst)
                    >= mistlib_core::signaling::nostr::TARGET_PEERS)
        {
            return Ok(false);
        }
        self.publish_frame_guarded(
            event_frame_json(&event)?,
            reserved.then_some((receiver_pubkey, expected_epoch, proactive)),
        )
        .await
    }

    async fn next_outgoing_sequence(&self, receiver_pubkey: &str) -> u64 {
        let mut sequences = self.outgoing_sequences.lock().await;
        next_outgoing_sequence(&mut sequences, receiver_pubkey)
    }

    pub(super) async fn send_request_to_pubkey(
        &self,
        receiver_pubkey: &str,
        room_id: &str,
    ) -> mistlib_core::error::Result<()> {
        let epoch = self.session_epoch();
        {
            let mut exchanges = self.exchanges.lock().await;
            if !self.session_is_current(epoch)
                || !exchanges.request(receiver_pubkey, web_time::Instant::now())
            {
                return Ok(());
            }
        }
        #[cfg(not(test))]
        let delay = discovery_request_jitter();
        #[cfg(test)]
        let delay = self.request_jitter;
        self.queue_request(
            receiver_pubkey,
            room_id,
            epoch,
            DiscoveryPriority::Ordinary,
            delay,
        )
        .await;
        Ok(())
    }

    // Bootstrap probes already have a deadline/backoff and a small hard cap.
    // Do not add the normal responder jitter to each candidate sequentially.
    pub(super) async fn publish_request_to_pubkey(
        &self,
        receiver_pubkey: &str,
        room_id: &str,
    ) -> mistlib_core::error::Result<()> {
        let epoch = self.session_epoch();
        {
            let mut outbox = self.outbox.lock().await;
            if !self.session_is_current(epoch) || outbox.promote_reply(receiver_pubkey) {
                return Ok(());
            }
        }
        {
            let mut exchanges = self.exchanges.lock().await;
            if !self.session_is_current(epoch)
                || !exchanges.request(receiver_pubkey, web_time::Instant::now())
            {
                return Ok(());
            }
        }
        self.queue_request(
            receiver_pubkey,
            room_id,
            epoch,
            DiscoveryPriority::Reply,
            web_time::Duration::ZERO,
        )
        .await;
        Ok(())
    }

    pub(super) async fn publish_reserved_request(
        &self,
        receiver_pubkey: &str,
        room_id: &str,
        expected_epoch: u64,
        proactive: bool,
    ) -> mistlib_core::error::Result<bool> {
        let request = SignalingData {
            sender_id: self.local_node_id.clone(),
            receiver_id: NodeId::broadcast(),
            room_id: room_id.to_string(),
            data: String::new(),
            signaling_type: SignalingType::Request,
        };
        self.publish_message_in_session(receiver_pubkey, &request, expected_epoch, proactive, true)
            .await
    }
}
