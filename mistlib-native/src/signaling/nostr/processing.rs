use super::NostrSignaler;
use mistlib_core::error::MistError;
use mistlib_core::signaling::nostr::{
    accept_message_order as accept_nostr_message_order,
    accept_sender_for_payload as accept_nostr_sender_for_payload, decode_discovery_event,
    decode_message_event, is_broadcast_sentinel_message, is_room_mailbox_message,
    record_discovery_and_should_request, MessageOrderAcceptance,
    DEFAULT_MAX_DISCOVERY_RESPONDERS_PER_PEER,
};
use mistlib_core::signaling::{MessageContent, SignalingData, SignalingType};
use tokio::sync::mpsc;

impl NostrSignaler {
    /// Gate on whether an incoming non-`Request` payload's claimed sender is
    /// admissible, on top of `mistlib_core`'s pubkey-identity check
    /// (already-known-by-this-exact-pubkey, or previously `Request`-ed).
    ///
    /// A peer that restarted arrives under a brand-new signaling pubkey that
    /// we have never requested and that isn't yet bound to its node id in the
    /// discovery table -- the core gate alone would drop its
    /// Offer/Answer/Candidate right here, before `process_event` ever reaches
    /// the rebind logic below, making the whole epoch mechanism inert for
    /// exactly the payloads that matter most (an `Offer` is never a
    /// `Request` -- whichever side is the deterministic WebRTC offerer for a
    /// pair sends one as its first message after restarting, and that
    /// decision is a raw `NodeId` comparison entirely independent of the
    /// Nostr discovery-rank protocol that decides who sends `Request`s). So
    /// also admit a sender whose payload proves it is a *newer session of a
    /// node id we already know*: `data.sender_id` is already bound to some
    /// (different) pubkey, and the peer-declared `sender_epoch` is strictly
    /// newer than the epoch we last recorded for that binding. This mirrors
    /// the acceptance condition `DiscoveryTable::bind_node_with_epoch` itself
    /// uses, so nothing is admitted here that the subsequent bind wouldn't
    /// also accept as a legitimate rebind.
    async fn accept_sender_for_payload(
        &self,
        sender_pubkey: &str,
        data: &mistlib_core::signaling::SignalingData,
        sender_epoch: Option<u64>,
    ) -> bool {
        let sender_was_requested = self.requested_pubkeys.lock().await.contains(sender_pubkey);
        let accepted = {
            let mut table = self.discovery_table.lock().await;
            if accept_nostr_sender_for_payload(
                &mut table,
                sender_was_requested,
                sender_pubkey,
                data,
            ) {
                true
            } else if table.pubkey_for_node(&data.sender_id).is_none() {
                false
            } else {
                let stored_epoch = table.epoch_for_node(&data.sender_id);
                matches!(
                    sender_epoch,
                    Some(candidate) if stored_epoch.is_none_or(|stored| candidate > stored)
                )
            }
        };
        if !accepted {
            tracing::warn!(
                "NostrSignaler: dropping unexpected signaling payload from {} node={} type={:?}",
                sender_pubkey,
                data.sender_id.0,
                data.signaling_type
            );
        }
        accepted
    }

    async fn accept_message_order(
        &self,
        sender_pubkey: &str,
        message_id: Option<&str>,
        sequence: Option<u64>,
    ) -> bool {
        let outcome = {
            let mut dedupe = self.message_dedupe.lock().await;
            let mut sequences = self.incoming_sequences.lock().await;
            accept_nostr_message_order(
                &mut dedupe,
                &mut sequences,
                sender_pubkey,
                message_id,
                sequence,
            )
        };
        // A silent drop of signaling must not exist on this path: every
        // rejection outcome is logged, and StaleSequence is a WARN (mirroring
        // Gap) because it is the only outcome here that actually discards a
        // message the sender considered live.
        match outcome {
            MessageOrderAcceptance::Gap { last, sequence } => {
                tracing::warn!(
                    "NostrSignaler: message sequence gap from {}: last={} next={}",
                    sender_pubkey,
                    last,
                    sequence
                );
            }
            MessageOrderAcceptance::ReorderedWithinWindow { last, sequence } => {
                tracing::debug!(
                    "NostrSignaler: accepted reordered message within window from {}: last={} sequence={}",
                    sender_pubkey,
                    last,
                    sequence
                );
            }
            MessageOrderAcceptance::StaleSequence { last, sequence } => {
                tracing::warn!(
                    "NostrSignaler: dropping stale message (beyond reorder window) from {}: last={} sequence={}",
                    sender_pubkey,
                    last,
                    sequence
                );
            }
            MessageOrderAcceptance::Accepted | MessageOrderAcceptance::DuplicateMessageId => {}
        }
        outcome.is_accepted()
    }

    pub(super) async fn process_event(
        &self,
        event: mistlib_core::signaling::nostr::NostrEvent,
        incoming_tx: mpsc::Sender<MessageContent>,
    ) -> mistlib_core::error::Result<()> {
        let session_epoch = self.session_epoch();
        let identity = self.current_identity().await;
        if event.pubkey == identity.public_key {
            return Ok(());
        }

        if event.kind == self.codec_config.discovery_kind {
            let Some(room_id) = self.current_room_id().await else {
                return Ok(());
            };
            let decoded =
                decode_discovery_event(&self.codec_config, &self.crypto, &event, &room_id)?;
            if self.current_room_id().await.as_deref() != Some(room_id.as_str())
                || !self.session_is_current(session_epoch)
            {
                return Ok(());
            }
            {
                let mut dedupe = self.dedupe.lock().await;
                if !dedupe.check_and_insert(&event.id) {
                    return Ok(());
                }
            }
            // NOTE: a peer that disconnected and rejoined advertises a fresh
            // `signaling_pubkey` (its identity is regenerated on every
            // restart) alongside a newer `joined_at`. Detecting that here
            // used to be attempted by keying `peer_sessions` on
            // `decoded.signaling_pubkey`, but that can never fire: the
            // pubkey is exactly the value that changes on rejoin, so
            // `sessions.get(&decoded.signaling_pubkey)` only ever sees a
            // brand-new key and never observes "same key, newer value".
            // Worse, `DecodedDiscovery` never carries the peer's `NodeId` at
            // all (discovery events are anonymous-by-pubkey; that's the
            // whole reason a `Request` exists, to learn the sender's node
            // id), so there was no key available here that could have
            // worked. Real rejoin detection now happens where it belongs --
            // keyed by `NodeId`, not by the very pubkey that rotates -- in
            // the message-processing branch below via
            // `DiscoveryTable::bind_node_with_epoch`'s `BindOutcome::Rebound`.
            let should_request = {
                let mut table = self.discovery_table.lock().await;
                if !self.session_is_current(session_epoch) {
                    return Ok(());
                }
                record_discovery_and_should_request(
                    &self.codec_config,
                    &mut table,
                    &decoded,
                    &room_id,
                    &identity.public_key,
                    DEFAULT_MAX_DISCOVERY_RESPONDERS_PER_PEER,
                )
            };
            if !self.session_is_current(session_epoch) {
                return Ok(());
            }
            let request_pubkeys = {
                let mut requested = self.requested_pubkeys.lock().await;
                if !self.session_is_current(session_epoch) {
                    return Ok(());
                }
                if should_request && requested.insert(decoded.signaling_pubkey.clone()) {
                    vec![decoded.signaling_pubkey.clone()]
                } else {
                    Vec::new()
                }
            };
            for pubkey in request_pubkeys {
                if self.current_room_id().await.as_deref() != Some(room_id.as_str())
                    || !self.session_is_current(session_epoch)
                {
                    return Ok(());
                }
                self.send_request_to_pubkey(&pubkey, &room_id).await?;
            }
            return Ok(());
        }

        if event.kind == self.codec_config.message_kind {
            let Some(room_id) = self.current_room_id().await else {
                return Ok(());
            };
            let decoded = match decode_message_event(
                &self.codec_config,
                &self.crypto,
                &identity,
                &self.local_node_id,
                &event,
                &room_id,
            ) {
                Ok(decoded) => decoded,
                Err(MistError::Signaling(err))
                    if err == "invalid encrypted Nostr payload"
                        && (is_room_mailbox_message(&self.codec_config, &event, &room_id)
                            || is_broadcast_sentinel_message(
                                &self.codec_config,
                                &event,
                                &room_id,
                            )) =>
                {
                    let mut dedupe = self.dedupe.lock().await;
                    dedupe.check_and_insert(&event.id);
                    return Ok(());
                }
                // The event's `p` tag names a different peer. A correctly
                // filtering relay should never deliver this to us, but a
                // misbehaving/legacy relay might; treat it the same as a
                // room-mailbox message that wasn't for us rather than a hard
                // error.
                Err(MistError::Signaling(err))
                    if err == "Nostr message receiver pubkey mismatch" =>
                {
                    let mut dedupe = self.dedupe.lock().await;
                    dedupe.check_and_insert(&event.id);
                    return Ok(());
                }
                Err(err) => return Err(err),
            };
            if self.current_room_id().await.as_deref() != Some(room_id.as_str())
                || !self.session_is_current(session_epoch)
            {
                return Ok(());
            }
            {
                let mut dedupe = self.dedupe.lock().await;
                if !dedupe.check_and_insert(&event.id) {
                    return Ok(());
                }
            }
            let mut incoming = decoded.data;
            // `Rejoin` is locally synthesized (see `SignalingType::Rejoin`'s
            // doc comment) and must never be accepted from the wire: a
            // remote peer must never be able to make us tear down a live
            // connection just by publishing a crafted message.
            if incoming.signaling_type.is_local_only() {
                tracing::warn!(
                    "NostrSignaler: dropping wire-delivered {:?} from {} (node={}) -- this \
                     signaling type is local-only and must never arrive from a relay",
                    incoming.signaling_type,
                    decoded.sender_pubkey,
                    incoming.sender_id.0
                );
                return Ok(());
            }
            let sender_was_requested = self
                .requested_pubkeys
                .lock()
                .await
                .contains(&decoded.sender_pubkey);
            if !self
                .accept_sender_for_payload(
                    &decoded.sender_pubkey,
                    &incoming,
                    decoded.sender_joined_at,
                )
                .await
            {
                return Ok(());
            }
            if !self
                .accept_message_order(
                    &decoded.sender_pubkey,
                    decoded.message_id.as_deref(),
                    decoded.sequence,
                )
                .await
            {
                return Ok(());
            }
            if !self.session_is_current(session_epoch) {
                return Ok(());
            }
            let (reply_pubkey, rebound_from) = {
                let sender_rank = self
                    .codec_config
                    .topology_rank(&room_id, &decoded.sender_pubkey);
                let mut table = self.discovery_table.lock().await;
                if !self.session_is_current(session_epoch) {
                    return Ok(());
                }
                // Epoch-aware bind: `decoded.sender_joined_at` (this sender's
                // session epoch, riding on the message envelope -- see
                // `publish_message_to_pubkey`) lets a pubkey change be
                // accepted as a legitimate rejoin (peer restarted, same
                // host-assigned NodeId, fresh signaling identity) even when
                // `allow_rebind` (the pre-existing `sender_was_requested &&
                // Request` escape hatch, unchanged below) doesn't apply. See
                // `DiscoveryTable::bind_node_with_epoch`'s doc comment for the
                // acceptance rule and its security trade-off.
                let outcome = table.bind_node_with_epoch(
                    incoming.sender_id.clone(),
                    decoded.sender_pubkey.clone(),
                    decoded.expires_at,
                    sender_rank,
                    decoded.sender_joined_at,
                    sender_was_requested && incoming.signaling_type == SignalingType::Request,
                )?;
                // A signaling message from this sender just passed validation,
                // dedupe, and ordering checks, so it is proof of life right now.
                // Renew its discovery entry using our own clock rather than
                // relying solely on the sender-declared `decoded.expires_at`,
                // so an active peer never lapses out of `node_to_pubkey` due to
                // a missed discovery re-announce cycle mid-exchange.
                table.touch_node(&incoming.sender_id, self.codec_config.ttl_seconds);
                let reply_pubkey = if !outcome.is_known()
                    && incoming.signaling_type == SignalingType::Request
                    && incoming.receiver_id.is_broadcast()
                {
                    Some(decoded.sender_pubkey.clone())
                } else {
                    None
                };
                (reply_pubkey, outcome.rebound_from().map(str::to_string))
            };
            if let Some(previous_pubkey) = rebound_from {
                tracing::info!(
                    "NostrSignaler: node {} rebound from pubkey {} to {} -- peer restarted \
                     (rejoin); purging stale per-peer state and notifying the transport",
                    incoming.sender_id.0,
                    previous_pubkey,
                    decoded.sender_pubkey
                );
                // Every per-peer map keyed by the now-dead pubkey must be
                // purged: it belongs to a signaling identity that no longer
                // exists, and leaving it behind would let stale sequence
                // counters / request bookkeeping / session epochs bleed into
                // the peer's fresh session.
                self.requested_pubkeys.lock().await.remove(&previous_pubkey);
                self.incoming_sequences
                    .lock()
                    .await
                    .remove(&previous_pubkey);
                self.outgoing_sequences
                    .lock()
                    .await
                    .remove(&previous_pubkey);
                self.peer_sessions.lock().await.remove(&previous_pubkey);

                if self.current_room_id().await.as_deref() != Some(room_id.as_str())
                    || !self.session_is_current(session_epoch)
                {
                    return Ok(());
                }
                // Injected into the local incoming stream BEFORE the
                // triggering message below, so the transport tears down the
                // stale peer connection before it ever sees the peer's real
                // Offer/Request that immediately follows on this same
                // ordered stream. See `SignalingType::Rejoin`'s doc comment.
                let rejoin = SignalingData {
                    sender_id: incoming.sender_id.clone(),
                    receiver_id: self.local_node_id.clone(),
                    room_id: room_id.clone(),
                    data: decoded
                        .sender_joined_at
                        .map(|epoch| epoch.to_string())
                        .unwrap_or_else(|| "0".to_string()),
                    signaling_type: SignalingType::Rejoin,
                };
                incoming_tx
                    .send(MessageContent::Data(rejoin))
                    .await
                    .map_err(|e| {
                        mistlib_core::error::MistError::Signaling(format!(
                            "NostrSignaler: incoming channel closed: {e}"
                        ))
                    })?;
            }
            if incoming.receiver_id.is_broadcast() {
                incoming.receiver_id = self.local_node_id.clone();
            }
            if self.current_room_id().await.as_deref() != Some(room_id.as_str())
                || !self.session_is_current(session_epoch)
            {
                return Ok(());
            }
            incoming_tx
                .send(MessageContent::Data(incoming))
                .await
                .map_err(|e| {
                    mistlib_core::error::MistError::Signaling(format!(
                        "NostrSignaler: incoming channel closed: {e}"
                    ))
                })?;
            if let Some(pubkey) = reply_pubkey {
                let first_request = { self.requested_pubkeys.lock().await.insert(pubkey.clone()) };
                if !first_request {
                    return Ok(());
                }
                if self.current_room_id().await.as_deref() != Some(room_id.as_str())
                    || !self.session_is_current(session_epoch)
                {
                    return Ok(());
                }
                self.send_request_to_pubkey(&pubkey, &room_id).await?;
            }
        }
        Ok(())
    }
}
