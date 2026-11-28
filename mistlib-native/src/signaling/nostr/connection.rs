use super::NostrSignaler;
use futures::future::join_all;
use futures_util::{SinkExt, StreamExt};
use mistlib_core::signaling::nostr::closed::{
    is_permanent_closed_reason, ClosedAction, ClosedResubscribe,
};
use mistlib_core::signaling::nostr::{
    discovery_filter, message_filter, parse_relay_message, req_frame_json, split_local_relays,
    RelayMessage,
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

/// Per-attempt deadline used by the reconnect supervisor of an already-opened
/// relay (`spawn_relay_supervisor`). The initial connection uses the bounded
/// retry schedule below instead.
pub(super) const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline of one initial connection attempt (TCP + TLS + WebSocket upgrade).
///
/// A healthy handshake is ~2-3 RTT. 3.5s still lets a lost SYN or upgrade
/// segment be recovered by the kernel's first two retransmissions (1s + 2s
/// RTO), but abandons a connection whose retransmission timer has backed off
/// further: a fresh TCP connection is then more likely to finish first.
pub(super) const RELAY_CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(3500);
/// Overall per-relay budget of the initial connection, measured from the start
/// of the first attempt. Only a little above the former single 10s attempt so
/// that a dead relay is still reported promptly.
const RELAY_CONNECT_BUDGET: Duration = Duration::from_secs(12);
/// Upper bound on initial attempts per relay (bounds fast failures such as a
/// refused connection, which would otherwise loop until the budget runs out).
pub(super) const RELAY_CONNECT_MAX_ATTEMPTS: u32 = 4;
/// Delay before the first retry; doubles per retry, with ±20% jitter.
const RELAY_CONNECT_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
/// Do not start an attempt that would have less time than this to finish.
const RELAY_CONNECT_MIN_ATTEMPT: Duration = Duration::from_secs(1);

type RelayStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Why one connection attempt failed, and whether a new attempt may help.
#[derive(Debug)]
struct ConnectFailure {
    error: crate::error::MistError,
    /// Short cause for the retry log. Never contains the relay URL.
    reason: String,
    retryable: bool,
}

impl ConnectFailure {
    fn cancelled() -> Self {
        Self {
            error: crate::error::MistError::Network("NostrSignaler: connection cancelled".into()),
            reason: "cancelled".into(),
            retryable: false,
        }
    }

    fn timed_out(relay: &str, timeout: Duration) -> Self {
        Self {
            error: crate::error::MistError::Network(format!(
                "NostrSignaler: connect to {relay} timed out"
            )),
            reason: format!("timed out after {}ms", timeout.as_millis()),
            retryable: true,
        }
    }

    fn handshake(err: tokio_tungstenite::tungstenite::Error) -> Self {
        let retryable = is_retryable_handshake_error(&err);
        // Only transport errors are retried and logged; their Display carries
        // the OS error, not the URL.
        let reason = err.to_string();
        Self {
            error: crate::error::MistError::Network(reason.clone()),
            reason,
            retryable,
        }
    }
}

/// Classifies a handshake error. Only transport-level failures that happen
/// before the HTTP upgrade completes are retried (refused, reset, aborted,
/// EOF, timed out); invalid URLs, DNS failures, TLS errors and an HTTP response
/// refusing the upgrade are permanent.
fn is_retryable_handshake_error(err: &tokio_tungstenite::tungstenite::Error) -> bool {
    use std::io::ErrorKind;
    use tokio_tungstenite::tungstenite::error::{Error, ProtocolError};
    match err {
        Error::Io(io) => matches!(
            io.kind(),
            ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::UnexpectedEof
                | ErrorKind::TimedOut
                | ErrorKind::BrokenPipe
                | ErrorKind::NotConnected
                | ErrorKind::Interrupted
        ),
        Error::ConnectionClosed => true,
        Error::Protocol(
            ProtocolError::HandshakeIncomplete | ProtocolError::ResetWithoutClosingHandshake,
        ) => true,
        _ => false,
    }
}

/// Retry schedule of the initial connection. After `attempts_done` failed
/// attempts and `elapsed` since the first one started, returns the delay
/// before the next attempt and that attempt's timeout, or `None` once the
/// attempt cap or the budget is exhausted. `jitter` is in `[0, 1)`.
fn next_initial_attempt(
    attempts_done: u32,
    elapsed: Duration,
    jitter: f64,
) -> Option<(Duration, Duration)> {
    if attempts_done >= RELAY_CONNECT_MAX_ATTEMPTS {
        return None;
    }
    let delay = if attempts_done == 0 {
        Duration::ZERO
    } else {
        let base = RELAY_CONNECT_RETRY_BASE_DELAY * 2u32.pow(attempts_done - 1);
        base.mul_f64(0.8 + 0.4 * jitter.clamp(0.0, 1.0))
    };
    let remaining = RELAY_CONNECT_BUDGET.checked_sub(elapsed + delay)?;
    if remaining < RELAY_CONNECT_MIN_ATTEMPT {
        return None;
    }
    Some((delay, remaining.min(RELAY_CONNECT_ATTEMPT_TIMEOUT)))
}

/// Relay URL for logs reduced to `scheme://host:port`: userinfo, path, query
/// and fragment are dropped because any of them may carry credentials.
fn redact_relay(relay: &str) -> String {
    let (scheme, rest) = match relay.find("://") {
        Some(idx) => (&relay[..idx + 3], &relay[idx + 3..]),
        None => ("", relay),
    };
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let host = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    format!("{scheme}{host}")
}

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
        *self.retry_incoming.lock().await = Some(incoming_tx.clone());
        let cancel = CancellationToken::new();
        {
            let mut lock = self.reconnect_cancel.lock().await;
            if let Some(previous) = lock.replace(cancel.clone()) {
                previous.cancel();
            }
        }
        self.senders.lock().await.clear();

        let local = if self.prefer_local_relays {
            self.local_inline_relays()
        } else {
            Vec::new()
        };
        // Loopback relays in the local-first stages get a single attempt so a
        // dead local relay falls back to the remote stage without delay; the
        // remote stage retries (see `connect_relay_with_retry`).
        if !local.is_empty()
            && self
                .connect_relay_set(&local, &incoming_tx, &cancel, false)
                .await
        {
            return Ok(());
        }
        if self.prefer_local_relays && cancel.is_cancelled() {
            return Err(Self::connection_failed());
        }
        let relays = if self.prefer_local_relays {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Self::connection_failed()),
                result = self.resolve_relays() => result?,
            }
        } else {
            self.resolve_relays().await?
        };
        let mut remaining: Vec<_> = relays
            .into_iter()
            .filter(|relay| !local.contains(relay))
            .collect();
        if self.prefer_local_relays {
            // The fetched relay list may itself contain a loopback relay (the
            // default list does). Apply the same local-first rule to it.
            let (listed_local, others) = split_local_relays(remaining);
            if !listed_local.is_empty()
                && self
                    .connect_relay_set(&listed_local, &incoming_tx, &cancel, false)
                    .await
            {
                return Ok(());
            }
            if cancel.is_cancelled() {
                return Err(Self::connection_failed());
            }
            remaining = others;
        }
        if self
            .connect_relay_set(&remaining, &incoming_tx, &cancel, true)
            .await
        {
            Ok(())
        } else {
            Err(Self::connection_failed())
        }
    }

    fn connection_failed() -> crate::error::MistError {
        crate::error::MistError::Network(
            "NostrSignaler: failed to connect to any relay".to_string(),
        )
    }

    /// Opens every relay of one stage in parallel. With `retry`, each relay
    /// follows the bounded schedule of [`next_initial_attempt`]; a relay stops
    /// retrying (after its in-flight attempt) once another relay of the set
    /// has opened, so one dead relay does not hold the join open for the
    /// whole budget.
    async fn connect_relay_set(
        &self,
        relays: &[String],
        incoming_tx: &mpsc::Sender<MessageContent>,
        cancel: &CancellationToken,
        retry: bool,
    ) -> bool {
        let any_connected = CancellationToken::new();
        let results = join_all(relays.iter().map(|relay| {
            let incoming_tx = incoming_tx.clone();
            let cancel = cancel.clone();
            let any_connected = any_connected.clone();
            async move {
                let result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return false,
                    result = self.connect_relay_with_retry(
                        relay, incoming_tx.clone(), cancel.clone(), retry, &any_connected,
                    ) => result,
                };
                match result {
                    Ok(disconnected) => {
                        any_connected.cancel();
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
        !cancel.is_cancelled() && results.into_iter().any(|connected| connected)
    }

    /// Initial connection to one relay: a fresh TCP connection per attempt,
    /// retried on transient handshake failures within the budget.
    async fn connect_relay_with_retry(
        &self,
        relay: &str,
        incoming_tx: mpsc::Sender<MessageContent>,
        cancel: CancellationToken,
        retry: bool,
        any_connected: &CancellationToken,
    ) -> crate::error::Result<oneshot::Receiver<()>> {
        let started = Instant::now();
        let mut attempts = 0;
        let mut attempt_timeout = RELAY_CONNECT_ATTEMPT_TIMEOUT;
        loop {
            attempts += 1;
            let failure = match self.open_relay(relay, attempt_timeout, &cancel).await {
                Ok(ws) => return self.attach_relay(relay, ws, incoming_tx, cancel).await,
                Err(failure) => failure,
            };
            if !retry || !failure.retryable || any_connected.is_cancelled() {
                return Err(failure.error);
            }
            let Some((delay, timeout)) =
                next_initial_attempt(attempts, started.elapsed(), rand::random())
            else {
                return Err(failure.error);
            };
            tracing::info!(
                "NostrSignaler: relay connect retry attempt={} max_attempts={} relay={} delay_ms={} timeout_ms={} elapsed_ms={} cause={}",
                attempts + 1,
                RELAY_CONNECT_MAX_ATTEMPTS,
                redact_relay(relay),
                delay.as_millis(),
                timeout.as_millis(),
                started.elapsed().as_millis(),
                failure.reason
            );
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(failure.error),
                _ = any_connected.cancelled() => return Err(failure.error),
                _ = tokio::time::sleep(delay) => {}
            }
            attempt_timeout = timeout;
        }
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
        let ws = self
            .open_relay(relay, RELAY_CONNECT_TIMEOUT, &cancel)
            .await
            .map_err(|failure| failure.error)?;
        self.attach_relay(relay, ws, incoming_tx, cancel).await
    }

    /// One handshake attempt on a fresh TCP connection.
    async fn open_relay(
        &self,
        relay: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<RelayStream, ConnectFailure> {
        tracing::info!("NostrSignaler: connecting to {}", relay);
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ConnectFailure::cancelled()),
            result = tokio::time::timeout(timeout, connect_async(relay)) => result,
        };
        match result {
            Err(_) => Err(ConnectFailure::timed_out(relay, timeout)),
            Ok(Err(err)) => Err(ConnectFailure::handshake(err)),
            Ok(Ok((ws_stream, _))) => Ok(ws_stream),
        }
    }

    /// Wires an opened relay connection into the signaler (subscribe,
    /// discovery, writer/reader tasks).
    async fn attach_relay(
        &self,
        relay: &str,
        ws_stream: RelayStream,
        incoming_tx: mpsc::Sender<MessageContent>,
        cancel: CancellationToken,
    ) -> crate::error::Result<oneshot::Receiver<()>> {
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
        let writer_governor = self.governor.clone();
        writer_governor
            .lock()
            .unwrap()
            .open(relay, web_time::Instant::now());
        let writer_notify = self.governor_notify.clone();
        let writer_measurements = self.measurements.clone();
        let writer_relay = relay.to_owned();
        tokio::spawn(async move {
            let mut ping_interval = tokio::time::interval(RELAY_PING_INTERVAL);
            ping_interval.tick().await; // first tick fires immediately; skip it
            'writer: loop {
                let notified = writer_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let wakeup = writer_governor
                    .lock()
                    .unwrap()
                    .next_wakeup(&writer_relay, web_time::Instant::now());
                let wait_for_work = async {
                    match wakeup {
                        Some(delay) => tokio::time::sleep(delay).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    _ = writer_cancel.cancelled() => break,
                    maybe_frame = rx.recv() => {
                        let Some(frame) = maybe_frame else { break };
                        let control = writer_governor.lock().unwrap().enqueue(&writer_relay, frame, web_time::Instant::now());
                        if let Some(frame) = control {
                            let result = tokio::select! {
                                _ = writer_cancel.cancelled() => break 'writer,
                                result = write.send(Message::Text(frame.clone().into())) => result,
                            };
                            if let Err(err) = result { tracing::warn!("NostrSignaler: relay write failed: {err}"); break 'writer; }
                            writer_measurements.lock().unwrap().sent(&frame, &writer_relay, web_time::Instant::now());
                        }
                    }
                    _ = &mut notified => {}
                    _ = wait_for_work => {
                        // Drain all available priorities before choosing the next EVENT.
                        while let Ok(frame) = rx.try_recv() {
                            let control = writer_governor.lock().unwrap().enqueue(&writer_relay, frame, web_time::Instant::now());
                            if let Some(frame) = control {
                                let result = tokio::select! {
                                    _ = writer_cancel.cancelled() => break 'writer,
                                    result = write.send(Message::Text(frame.clone().into())) => result,
                                };
                                if result.is_err() { break 'writer; }
                                writer_measurements.lock().unwrap().sent(&frame, &writer_relay, web_time::Instant::now());
                            }
                        }
                        loop {
                            let frame = writer_governor.lock().unwrap().poll(&writer_relay, web_time::Instant::now());
                            let Some(frame) = frame else { break; };
                            let result = tokio::select! {
                                _ = writer_cancel.cancelled() => break 'writer,
                                result = write.send(Message::Text(frame.clone().into())) => result,
                            };
                            if let Err(err) = result { tracing::warn!("NostrSignaler: relay write failed: {err}"); break 'writer; }
                            writer_measurements.lock().unwrap().sent(&frame, &writer_relay, web_time::Instant::now());
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
            writer_governor.lock().unwrap().disconnected(&writer_relay);
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
        let reader_relay = relay.to_owned();
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
                        signaler.observe_relay_message(&parsed, &reader_relay);
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

    fn observe_relay_message(&self, message: &RelayMessage, relay: &str) {
        self.governor
            .lock()
            .unwrap()
            .observe(relay, message, web_time::Instant::now());
        self.governor_notify.notify_waiters();
        let mut stats = self.measurements.lock().unwrap();
        match message {
            RelayMessage::Ok {
                event_id,
                accepted,
                message,
            } => stats.status(
                event_id,
                *accepted,
                message,
                relay,
                web_time::Instant::now(),
            ),
            RelayMessage::Event {
                subscription_id,
                event,
            } => {
                stats.received();
                tracing::debug!("[NostrReceive] relay={relay} subscription={} event_id={} kind={} pubkey={} created_at={}", subscription_id, event.id, event.kind, event.pubkey, event.created_at);
            }
            RelayMessage::Eose { subscription_id } => {
                tracing::debug!("[NostrEOSE] relay={relay} subscription={subscription_id}")
            }
            _ => {}
        }
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
            accepted: false,
            event_id,
            message,
        } if !mistlib_core::signaling::nostr::governor::rate_limited(&message) => {
            tracing::warn!("NostrSignaler: relay rejected event {event_id}: {message}");
        }
        RelayMessage::Ok { .. } => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_schedule_fits_three_stalled_attempts_in_the_budget() {
        let mut elapsed = Duration::ZERO;
        let mut attempts = 0;
        let mut timeouts = Vec::new();
        while let Some((delay, timeout)) = next_initial_attempt(attempts, elapsed, 1.0) {
            assert!(timeout <= RELAY_CONNECT_ATTEMPT_TIMEOUT);
            elapsed += delay + timeout;
            attempts += 1;
            timeouts.push(timeout);
        }
        assert_eq!(timeouts, vec![RELAY_CONNECT_ATTEMPT_TIMEOUT; 3]);
        assert!(elapsed <= RELAY_CONNECT_BUDGET);
    }

    #[test]
    fn initial_schedule_caps_attempts_and_jitters_delays() {
        let exhausted = next_initial_attempt(RELAY_CONNECT_MAX_ATTEMPTS, Duration::ZERO, 0.5);
        assert!(exhausted.is_none());
        let (delay, _) = next_initial_attempt(1, Duration::ZERO, 0.0).unwrap();
        assert_eq!(delay, Duration::from_millis(200));
        let (delay, _) = next_initial_attempt(1, Duration::ZERO, 1.0).unwrap();
        assert_eq!(delay, Duration::from_millis(300));
        // The last attempt is shortened to what is left of the budget.
        let late = Duration::from_secs(10);
        let (_, last) = next_initial_attempt(1, late, 0.0).unwrap();
        assert!(last < RELAY_CONNECT_ATTEMPT_TIMEOUT);
    }

    #[test]
    fn handshake_error_classification() {
        use std::io::{Error as IoError, ErrorKind};
        use tokio_tungstenite::tungstenite::error::{Error, UrlError};
        use tokio_tungstenite::tungstenite::http::Response;

        let retryable = |err: Error| is_retryable_handshake_error(&err);
        for kind in [
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
            ErrorKind::UnexpectedEof,
            ErrorKind::TimedOut,
        ] {
            assert!(retryable(Error::Io(IoError::from(kind))));
        }
        let dns = IoError::other("failed to lookup address");
        assert!(!retryable(Error::Io(dns)));
        assert!(!retryable(Error::Url(UrlError::UnsupportedUrlScheme)));
        let refused = Response::builder().status(403).body(None).unwrap();
        assert!(!retryable(Error::Http(Box::new(refused))));
    }

    #[test]
    fn relay_redaction_drops_credentials() {
        assert_eq!(
            redact_relay("wss://user:secret@relay.example:443/path/token?auth=x#f"),
            "wss://relay.example:443"
        );
        assert_eq!(redact_relay("ws://127.0.0.1:7777"), "ws://127.0.0.1:7777");
        assert_eq!(redact_relay("relay.example?x=1"), "relay.example");
    }
}
