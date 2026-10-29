use super::NostrSignaler;
use futures::future::join_all;
use futures_util::{SinkExt, StreamExt};
use mistlib_core::signaling::nostr::closed::{
    is_permanent_closed_reason, ClosedAction, ClosedResubscribe,
};
use mistlib_core::signaling::nostr::{
    discovery_filter, message_filter, parse_relay_message, req_frame_json, RelayMessage,
};
use mistlib_core::signaling::reconnect::ReconnectBackoff;
use mistlib_core::signaling::MessageContent;
use mistlib_core::stats::STATS;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::Instant;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tokio_util::sync::CancellationToken;

pub(super) const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct SubscriptionRetry {
    backoff: ClosedResubscribe,
    pending: Option<CancellationToken>,
    gave_up: bool,
}

impl SubscriptionRetry {
    fn on_accepted(&mut self) {
        self.backoff.on_accepted();
        self.cancel_pending();
    }

    fn cancel_pending(&mut self) {
        if let Some(cancel) = self.pending.take() {
            cancel.cancel();
        }
    }
}

type SubscriptionRetries = HashMap<String, Arc<Mutex<SubscriptionRetry>>>;

/// Interval between keepalive `Ping` frames sent on each relay connection.
///
/// Chosen well below typical proxy/NAT idle websocket timeouts (30-120s).
/// Without this, outbound relay traffic drops to one discovery refresh every
/// ~5-6 minutes after room join, which is far above those timeouts and leads
/// to silent relay disconnects that the supervisor cannot detect until the
/// next send fails.
#[cfg(not(test))]
const RELAY_PING_INTERVAL: Duration = Duration::from_secs(25);
#[cfg(test)]
const RELAY_PING_INTERVAL: Duration = Duration::from_millis(200);

impl NostrSignaler {
    pub async fn connect(
        &self,
        incoming_tx: mpsc::Sender<MessageContent>,
    ) -> crate::error::Result<()> {
        let cancel = CancellationToken::new();
        {
            let mut lock = self.reconnect_cancel.lock().await;
            if let Some(previous) = lock.replace(cancel.clone()) {
                previous.cancel();
            }
        }
        self.senders.lock().await.clear();

        let relays = self.resolve_relays().await?;
        let results = join_all(relays.iter().map(|relay| {
            let incoming_tx = incoming_tx.clone();
            let cancel = cancel.clone();
            async move {
                let result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return false,
                    result = self.connect_relay_once(relay, incoming_tx.clone(), cancel.clone()) => result,
                };
                match result {
                    Ok(disconnected) => {
                        self.spawn_relay_supervisor(
                            relay.clone(),
                            incoming_tx.clone(),
                            cancel.clone(),
                            disconnected,
                        );
                        true
                    }
                    Err(err) => {
                        tracing::warn!("NostrSignaler: connect to {} failed: {:?}", relay, err);
                        false
                    }
                }
            }
        }))
        .await;

        if cancel.is_cancelled() || !results.into_iter().any(|connected| connected) {
            return Err(crate::error::MistError::Network(
                "NostrSignaler: failed to connect to any relay".to_string(),
            ));
        }
        Ok(())
    }

    fn spawn_relay_supervisor(
        &self,
        relay: String,
        incoming_tx: mpsc::Sender<MessageContent>,
        cancel: CancellationToken,
        mut disconnected: oneshot::Receiver<()>,
    ) {
        let signaler = self.clone();
        let mut backoff = ReconnectBackoff::new();
        backoff.on_connected(web_time::Instant::now());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = &mut disconnected => {}
                }
                backoff.on_disconnected(web_time::Instant::now());

                // Consume each disconnect notification once, then keep
                // retrying until a new connection supplies a fresh receiver.
                loop {
                    let delay = backoff.next_delay();
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(delay) => {}
                    }

                    tracing::info!("NostrSignaler: reconnecting to {}", relay);
                    let result = tokio::select! {
                        _ = cancel.cancelled() => return,
                        result = signaler.connect_relay_once(&relay, incoming_tx.clone(), cancel.clone()) => result,
                    };
                    match result {
                        Ok(next_disconnected) => {
                            backoff.on_connected(web_time::Instant::now());
                            tracing::info!("NostrSignaler: reconnected to {}", relay);
                            disconnected = next_disconnected;
                            break;
                        }
                        Err(err) => {
                            tracing::warn!(
                                "NostrSignaler: reconnect to {} failed: {:?}",
                                relay,
                                err
                            );
                        }
                    }
                }
            }
        });
    }

    async fn connect_relay_once(
        &self,
        relay: &str,
        incoming_tx: mpsc::Sender<MessageContent>,
        cancel: CancellationToken,
    ) -> crate::error::Result<oneshot::Receiver<()>> {
        tracing::info!("NostrSignaler: connecting to {}", relay);
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(crate::error::MistError::Network("NostrSignaler: connection cancelled".into()));
            }
            result = tokio::time::timeout(RELAY_CONNECT_TIMEOUT, connect_async(relay)) => result,
        };
        let (ws_stream, _) = result
            .map_err(|_| {
                crate::error::MistError::Network(format!(
                    "NostrSignaler: connect to {relay} timed out"
                ))
            })?
            .map_err(|err| crate::error::MistError::Network(err.to_string()))?;
        let (mut write, mut read) = ws_stream.split();
        let (tx, mut rx) = mpsc::channel::<String>(1024);
        let (disconnected_tx, disconnected_rx) = oneshot::channel::<()>();
        let disconnected_tx = Arc::new(StdMutex::new(Some(disconnected_tx)));
        let connection_cancel = cancel.child_token();
        // Tracks the last time any frame (data, ping, pong, ...) was read from
        // the relay, so the writer's ping loop can notice a connection that
        // has gone completely silent despite our keepalive pings.
        let last_activity = Arc::new(StdMutex::new(Instant::now()));

        let room_id = self.current_room_id().await;
        if let Some(room_id) = room_id.as_deref() {
            self.subscribe(&tx, room_id).await?;
        }

        let reader_tx = tx.clone();
        {
            let mut senders = self.senders.lock().await;
            senders.retain(|tx| !tx.is_closed());
            senders.push(tx);
        }

        if let Some(room_id) = room_id.as_deref() {
            self.publish_discovery(room_id).await?;
        }

        let writer_cancel = connection_cancel.clone();
        let writer_cancel_on_exit = writer_cancel.clone();
        let writer_disconnected = disconnected_tx.clone();
        let writer_last_activity = last_activity.clone();
        tokio::spawn(async move {
            let mut ping_interval = tokio::time::interval(RELAY_PING_INTERVAL);
            ping_interval.tick().await; // first tick fires immediately; skip it
            loop {
                tokio::select! {
                    _ = writer_cancel.cancelled() => break,
                    maybe_frame = rx.recv() => {
                        let Some(frame) = maybe_frame else { break };
                        let result = tokio::select! {
                            _ = writer_cancel.cancelled() => break,
                            result = write.send(Message::Text(frame.into())) => result,
                        };
                        if let Err(err) = result {
                            tracing::warn!("NostrSignaler: relay write failed: {}", err);
                            break;
                        }
                    }
                    _ = ping_interval.tick() => {
                        let silent_for = writer_last_activity.lock().unwrap().elapsed();
                        if silent_for >= RELAY_PING_INTERVAL * 2 {
                            tracing::warn!(
                                "NostrSignaler: relay connection silent for {:?}; treating as dead",
                                silent_for
                            );
                            break;
                        }
                        let result = tokio::select! {
                            _ = writer_cancel.cancelled() => break,
                            result = write.send(Message::Ping(Vec::new().into())) => result,
                        };
                        if let Err(err) = result {
                            tracing::warn!("NostrSignaler: relay ping failed: {}", err);
                            break;
                        }
                    }
                }
            }
            writer_cancel_on_exit.cancel();
            if let Some(tx) = writer_disconnected.lock().unwrap().take() {
                let _ = tx.send(());
            }
        });

        let signaler = self.clone();
        let reader_cancel = connection_cancel.clone();
        let reader_cancel_on_exit = reader_cancel.clone();
        let reader_disconnected = disconnected_tx.clone();
        let reader_last_activity = last_activity.clone();
        tokio::spawn(async move {
            let mut retries = HashMap::new();
            loop {
                tokio::select! {
                    _ = reader_cancel.cancelled() => break,
                    maybe_msg = read.next() => {
                        let Some(msg) = maybe_msg else { break };
                        *reader_last_activity.lock().unwrap() = Instant::now();
                        let raw = match msg {
                            Ok(Message::Text(text)) => text.to_string(),
                            Ok(Message::Binary(bytes)) => match String::from_utf8(bytes.to_vec()) {
                                Ok(text) => text,
                                Err(err) => {
                                    tracing::warn!(
                                        "NostrSignaler: relay binary frame is not UTF-8: {}",
                                        err
                                    );
                                    continue;
                                }
                            },
                            Ok(Message::Close(frame)) => {
                                tracing::info!("NostrSignaler: relay closed: {:?}", frame);
                                break;
                            }
                            Err(err) => {
                                tracing::warn!("NostrSignaler: relay read failed: {}", err);
                                break;
                            }
                            _ => continue,
                        };
                        STATS.add_receive(raw.len() as u64);
                        let parsed = match parse_relay_message(&raw) {
                            Ok(Some(parsed)) => parsed,
                            Ok(None) => continue,
                            Err(err) => {
                                tracing::warn!("NostrSignaler: relay frame parse failed: {:?}", err);
                                continue;
                            }
                        };
                        tokio::select! {
                            _ = reader_cancel.cancelled() => break,
                            _ = signaler.handle_relay_message(
                                parsed, incoming_tx.clone(), &reader_tx, &reader_cancel, &mut retries,
                            ) => {}
                        }
                    }
                }
            }
            reader_cancel_on_exit.cancel();
            if let Some(tx) = reader_disconnected.lock().unwrap().take() {
                let _ = tx.send(());
            }
        });

        Ok(disconnected_rx)
    }

    async fn handle_relay_message(
        &self,
        message: RelayMessage,
        incoming_tx: mpsc::Sender<MessageContent>,
        relay_tx: &mpsc::Sender<String>,
        cancel: &CancellationToken,
        retries: &mut SubscriptionRetries,
    ) {
        match message {
            RelayMessage::Event {
                subscription_id,
                event,
            } => {
                if let Some(retry) = retries.get(&subscription_id) {
                    retry.lock().await.on_accepted();
                }
                if let Err(err) = self.process_event(event, incoming_tx).await {
                    tracing::warn!("NostrSignaler: event processing failed: {:?}", err);
                }
            }
            RelayMessage::Eose { subscription_id } => {
                if let Some(retry) = retries.get(&subscription_id) {
                    retry.lock().await.on_accepted();
                }
            }
            RelayMessage::Closed {
                subscription_id,
                message,
            } => {
                let Some(ids) = self.subscription_ids.lock().await.clone() else {
                    return;
                };
                retries.retain(|id, _| id == &ids.discovery || id == &ids.message);
                if subscription_id != ids.discovery && subscription_id != ids.message {
                    return;
                }
                let state = retries.entry(subscription_id.clone()).or_default().clone();
                let mut retry = state.lock().await;
                if retry.gave_up {
                    return;
                }
                if retry.pending.is_some() && !is_permanent_closed_reason(&message) {
                    return;
                }
                match retry.backoff.on_closed(&message) {
                    ClosedAction::GiveUp => {
                        retry.gave_up = true;
                        retry.cancel_pending();
                        tracing::warn!(
                            "NostrSignaler: giving up subscription {} on this connection: {}",
                            subscription_id,
                            message
                        );
                    }
                    ClosedAction::ResubscribeAfter(delay) => {
                        let signaler = self.clone();
                        let tx = relay_tx.clone();
                        let cancel = cancel.child_token();
                        retry.pending = Some(cancel.clone());
                        drop(retry);
                        tokio::spawn(async move {
                            tokio::select! {
                                _ = cancel.cancelled() => {},
                                _ = async {
                                    tokio::time::sleep(delay).await;
                                    // Serialize completion with the reader so an immediate
                                    // CLOSED response can reserve the next retry.
                                    let mut retry = state.lock().await;
                                    if cancel.is_cancelled() {
                                        return;
                                    }
                                    signaler.resubscribe_after_closed(&tx, &subscription_id).await;
                                    retry.pending = None;
                                } => {}
                            }
                        });
                    }
                }
            }
            status => log_relay_status(status),
        }
    }

    /// Sends the room's discovery/message REQ frames on `tx`.
    ///
    /// Reuses the room's persisted subscription ids (generating them on
    /// first use) so that re-invoking this — on reconnect, periodic
    /// resubscribe, or after a relay-issued CLOSED — replaces the relay-side
    /// filter for the same subscription instead of opening a new one
    /// (NIP-01 REQ semantics).
    pub(super) async fn subscribe(
        &self,
        tx: &mpsc::Sender<String>,
        room_id: &str,
    ) -> mistlib_core::error::Result<()> {
        let ids = self.current_subscription_ids().await;
        let identity = self.current_identity().await;
        let discovery = discovery_filter(&self.codec_config, room_id);
        let message = message_filter(&self.codec_config, room_id, &identity.public_key);
        let discovery_frame = req_frame_json(&ids.discovery, &[discovery])?;
        let message_frame = req_frame_json(&ids.message, &[message])?;
        tx.send(discovery_frame).await.map_err(|e| {
            mistlib_core::error::MistError::Signaling(format!(
                "NostrSignaler: subscribe failed: {e}"
            ))
        })?;
        tx.send(message_frame).await.map_err(|e| {
            mistlib_core::error::MistError::Signaling(format!(
                "NostrSignaler: subscribe failed: {e}"
            ))
        })?;
        Ok(())
    }

    /// Re-issues only the rejected subscription's REQ on `tx` after a
    /// `CLOSED` for one of our active subscriptions, so the peer keeps
    /// receiving events instead of silently losing the subscription.
    async fn resubscribe_after_closed(&self, tx: &mpsc::Sender<String>, subscription_id: &str) {
        let Some(room_id) = self.current_room_id().await else {
            return;
        };
        let ids = self.subscription_ids.lock().await.clone();
        let Some(ids) = ids else {
            return;
        };
        let filter = if subscription_id == ids.discovery {
            discovery_filter(&self.codec_config, &room_id)
        } else if subscription_id == ids.message {
            let identity = self.current_identity().await;
            message_filter(&self.codec_config, &room_id, &identity.public_key)
        } else {
            return;
        };
        let result = async {
            let frame = req_frame_json(subscription_id, &[filter])?;
            tx.send(frame)
                .await
                .map_err(|err| mistlib_core::error::MistError::Signaling(err.to_string()))
        }
        .await;
        if let Err(err) = result {
            tracing::warn!(
                "NostrSignaler: resubscribe after CLOSED failed for room {}: {:?}",
                room_id,
                err
            );
        }
    }
}

fn log_relay_status(message: RelayMessage) {
    match message {
        RelayMessage::Ok {
            event_id,
            accepted,
            message,
        } => {
            if !accepted {
                tracing::warn!(
                    "NostrSignaler: relay rejected event {}: {}",
                    event_id,
                    message
                );
            } else if !message.is_empty() {
                tracing::debug!(
                    "NostrSignaler: relay accepted event {}: {}",
                    event_id,
                    message
                );
            }
        }
        RelayMessage::Notice(message) => {
            tracing::warn!("NostrSignaler: relay notice: {}", message);
        }
        RelayMessage::Auth(challenge) => {
            tracing::warn!(
                "NostrSignaler: relay requested AUTH challenge {}; NIP-42 auth is not implemented",
                challenge
            );
        }
        RelayMessage::Eose { .. } | RelayMessage::Event { .. } | RelayMessage::Closed { .. } => {}
    }
}
