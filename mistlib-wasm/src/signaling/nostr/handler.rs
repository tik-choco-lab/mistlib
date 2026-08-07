use super::WasmNostrSignaler;
use mistlib_core::error::MistError;
use mistlib_core::signaling::nostr::{
    accept_message_order as accept_nostr_message_order,
    accept_sender_for_payload as accept_nostr_sender_for_payload, decode_discovery_event,
    decode_message_event, is_broadcast_sentinel_message, is_room_mailbox_message,
    record_discovery_and_should_request, NostrEvent, DEFAULT_MAX_DISCOVERY_RESPONDERS_PER_PEER,
};
use mistlib_core::signaling::{MessageContent, SignalingData, SignalingType};
use tokio::sync::mpsc;

impl WasmNostrSignaler {
    pub(super) fn process_event(
        &self,
        event: NostrEvent,
        incoming_tx: &mpsc::UnboundedSender<MessageContent>,
    ) -> mistlib_core::error::Result<()> {
        let identity = self.current_identity();
        if event.pubkey == identity.public_key {
            return Ok(());
        }

        if event.kind == self.codec_config.discovery_kind {
            let Some(room_id) = self
                .room_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            else {
                return Ok(());
            };
            let decoded =
                decode_discovery_event(&self.codec_config, &self.crypto, &event, &room_id)?;
            if !self.mark_seen_if_current(&event.id, &room_id) {
                return Ok(());
            }
            // A peer that disconnected and rejoined advertises a newer joined_at.
            // Forget our prior request for it so we re-request the fresh session
            // instead of silently suppressing the duplicate.
            if let Some(joined_at) = decoded.joined_at {
                let rejoined = {
                    let mut sessions = self.peer_sessions.lock().unwrap_or_else(|e| e.into_inner());
                    match sessions.get(&decoded.signaling_pubkey) {
                        Some(&previous) if joined_at > previous => {
                            sessions.insert(decoded.signaling_pubkey.clone(), joined_at);
                            true
                        }
                        Some(_) => false,
                        None => {
                            sessions.insert(decoded.signaling_pubkey.clone(), joined_at);
                            false
                        }
                    }
                };
                if rejoined {
                    self.requested_pubkeys
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&decoded.signaling_pubkey);
                    self.incoming_sequences
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&decoded.signaling_pubkey);
                    self.outgoing_sequences
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&decoded.signaling_pubkey);
                }
            }
            let should_request = {
                let mut table = self
                    .discovery_table
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                record_discovery_and_should_request(
                    &self.codec_config,
                    &mut table,
                    &decoded,
                    &room_id,
                    &identity.public_key,
                    DEFAULT_MAX_DISCOVERY_RESPONDERS_PER_PEER,
                )
            };
            let first_request = should_request
                && self
                    .requested_pubkeys
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(decoded.signaling_pubkey.clone());
            if first_request {
                self.send_request_to_pubkey(&decoded.signaling_pubkey, &room_id)?;
            }
            return Ok(());
        }

        if event.kind == self.codec_config.message_kind {
            self.process_message_event(event, incoming_tx)?;
        }
        Ok(())
    }

    fn process_message_event(
        &self,
        event: NostrEvent,
        incoming_tx: &mpsc::UnboundedSender<MessageContent>,
    ) -> mistlib_core::error::Result<()> {
        let Some(room_id) = self
            .room_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return Ok(());
        };
        let decoded = match decode_message_event(
            &self.codec_config,
            &self.crypto,
            &self.current_identity(),
            &self.local_node_id,
            &event,
            &room_id,
        ) {
            Ok(decoded) => decoded,
            Err(MistError::Signaling(err))
                if err == "invalid encrypted Nostr payload"
                    && (is_room_mailbox_message(&self.codec_config, &event, &room_id)
                        || is_broadcast_sentinel_message(&self.codec_config, &event, &room_id)) =>
            {
                let _ = self.mark_seen_if_current(&event.id, &room_id);
                return Ok(());
            }
            // The event's `p` tag names a different peer. A correctly
            // filtering relay should never deliver this to us, but a
            // misbehaving/legacy relay might; treat it the same as a
            // room-mailbox message that wasn't for us rather than a hard
            // error.
            Err(MistError::Signaling(err)) if err == "Nostr message receiver pubkey mismatch" => {
                let _ = self.mark_seen_if_current(&event.id, &room_id);
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        if !self.mark_seen_if_current(&event.id, &room_id) {
            return Ok(());
        }
        let mut incoming = decoded.data;
        // `Rejoin` is a locally-synthesized notification (see
        // `SignalingType::is_local_only`): the signaling layer emits it into
        // `incoming_tx` itself when it detects a rebind below, and it must
        // never be accepted from a relay. A remote must not be able to make
        // us tear down a live peer connection by forging one.
        if incoming.signaling_type == SignalingType::Rejoin {
            return Ok(());
        }
        let sender_was_requested = self
            .requested_pubkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&decoded.sender_pubkey);
        if !self.accept_sender_for_payload(
            &decoded.sender_pubkey,
            &incoming,
            decoded.sender_joined_at,
        ) {
            tracing::warn!(
                "Nostr signaling drop: reason=sender_admission sender_node={} sender_pubkey={} type={:?} sender_epoch={:?}",
                incoming.sender_id.0,
                decoded.sender_pubkey,
                incoming.signaling_type,
                decoded.sender_joined_at
            );
            return Ok(());
        }
        if !self.accept_message_order(
            &decoded.sender_pubkey,
            decoded.message_id.as_deref(),
            decoded.sequence,
        ) {
            return Ok(());
        }
        let (reply_pubkey, rebound_from) = {
            let mut table = self
                .discovery_table
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let sender_rank = self
                .codec_config
                .topology_rank(&room_id, &decoded.sender_pubkey);
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
            // relying solely on the sender-declared `decoded.expires_at`, so
            // an active peer never lapses out of `node_to_pubkey` due to a
            // missed discovery re-announce cycle mid-exchange.
            table.touch_node(&incoming.sender_id, self.codec_config.ttl_seconds);
            let reply_pubkey = if !outcome.is_known()
                && incoming.signaling_type == SignalingType::Request
                && incoming.receiver_id.is_broadcast()
            {
                Some(decoded.sender_pubkey.clone())
            } else {
                None
            };
            (reply_pubkey, outcome.rebound_from().map(str::to_owned))
        };
        // The node id just rebound from `previous_pubkey` to
        // `decoded.sender_pubkey`: the peer restarted (e.g. a browser
        // reload) and regenerated its temporary signaling identity while
        // keeping the same host-assigned node id. Purge every piece of
        // per-peer state keyed by the now-dead pubkey so it cannot leak
        // (stale dedupe/ordering state, a stale "already requested" flag, a
        // stale discovery session-epoch record), then notify the transport
        // layer via a synthetic `Rejoin` BEFORE the triggering message
        // itself is forwarded, so it tears down the stale peer connection
        // first.
        if let Some(previous_pubkey) = rebound_from {
            self.purge_peer_state(&previous_pubkey);
            if self.room_is_current(&room_id) {
                let new_epoch = decoded.sender_joined_at.unwrap_or(0);
                incoming_tx
                    .send(MessageContent::Data(SignalingData {
                        sender_id: incoming.sender_id.clone(),
                        receiver_id: self.local_node_id.clone(),
                        room_id: room_id.clone(),
                        data: new_epoch.to_string(),
                        signaling_type: SignalingType::Rejoin,
                    }))
                    .map_err(|e| {
                        mistlib_core::error::MistError::Signaling(format!(
                            "WasmNostrSignaler: incoming channel closed: {e}"
                        ))
                    })?;
            }
        }
        if incoming.receiver_id.is_broadcast() {
            incoming.receiver_id = self.local_node_id.clone();
        }
        if !self.room_is_current(&room_id) {
            return Ok(());
        }
        incoming_tx
            .send(MessageContent::Data(incoming))
            .map_err(|e| {
                mistlib_core::error::MistError::Signaling(format!(
                    "WasmNostrSignaler: incoming channel closed: {e}"
                ))
            })?;
        if let Some(pubkey) = reply_pubkey {
            let first_request = self
                .requested_pubkeys
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pubkey.clone());
            if first_request {
                self.send_request_to_pubkey(&pubkey, &room_id)?;
            }
        }
        Ok(())
    }

    /// Gate on whether an incoming non-`Request` payload's claimed sender is
    /// admissible, on top of `mistlib_core`'s pubkey-identity check
    /// (already-known-by-this-exact-pubkey, or previously `Request`-ed).
    ///
    /// A peer that reloaded its page arrives under a brand-new signaling
    /// pubkey that we have never requested and that isn't yet bound to its
    /// node id in the discovery table — the core gate alone would drop its
    /// Offer/Answer/Candidate before `process_message_event` ever reaches
    /// the rebind logic below, making the whole epoch mechanism inert for
    /// exactly the payloads that matter most (an Offer is never a
    /// `Request`). So also admit a sender whose payload proves it is a
    /// *newer session of a node id we already know*: `data.sender_id` is
    /// already bound to some (different) pubkey, and the peer-declared
    /// `sender_epoch` is strictly newer than the epoch we last recorded for
    /// that binding. This mirrors the acceptance condition
    /// `DiscoveryTable::bind_node_with_epoch` itself uses, so nothing is
    /// admitted here that the subsequent bind wouldn't also accept as a
    /// legitimate rebind.
    fn accept_sender_for_payload(
        &self,
        sender_pubkey: &str,
        data: &SignalingData,
        sender_epoch: Option<u64>,
    ) -> bool {
        let sender_was_requested = self
            .requested_pubkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(sender_pubkey);
        let mut table = self
            .discovery_table
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if accept_nostr_sender_for_payload(&mut table, sender_was_requested, sender_pubkey, data) {
            return true;
        }
        if table.pubkey_for_node(&data.sender_id).is_none() {
            return false;
        }
        let stored_epoch = table.epoch_for_node(&data.sender_id);
        matches!(sender_epoch, Some(candidate) if stored_epoch.is_none_or(|stored| candidate > stored))
    }

    fn accept_message_order(
        &self,
        sender_pubkey: &str,
        message_id: Option<&str>,
        sequence: Option<u64>,
    ) -> bool {
        let mut dedupe = self
            .message_dedupe
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut sequences = self
            .incoming_sequences
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let acceptance = accept_nostr_message_order(
            &mut dedupe,
            &mut sequences,
            sender_pubkey,
            message_id,
            sequence,
        );
        if !acceptance.is_accepted() {
            tracing::warn!(
                "Nostr signaling drop: reason=message_order sender_pubkey={} acceptance={:?}",
                sender_pubkey,
                acceptance
            );
        }
        acceptance.is_accepted()
    }

    fn mark_seen_if_current(&self, event_id: &str, room_id: &str) -> bool {
        if !self.room_is_current(room_id) {
            return false;
        }
        self.dedupe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check_and_insert(event_id)
    }

    pub(super) fn room_is_current(&self, room_id: &str) -> bool {
        self.room_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            == Some(room_id)
    }
}
