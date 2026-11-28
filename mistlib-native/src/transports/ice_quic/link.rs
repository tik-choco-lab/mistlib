//! A connected ICE+QUIC link: send/receive per [`DeliveryMethod`], state,
//! stats, close.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use mistlib_core::types::DeliveryMethod;
use tokio::sync::{mpsc, oneshot, watch};
use webrtc::ice::agent::Agent;
use webrtc::ice::state::ConnectionState as IceConnectionState;

use super::{close_code, IceQuicError, IceQuicState};

/// First byte of the bidirectional (ordered) stream.
pub(super) const STREAM_ORDERED: u8 = 0x00;
/// First byte of a per-message unidirectional stream.
const STREAM_MESSAGE: u8 = 0x01;
/// First byte of a media track announcement stream (unidirectional): the
/// announcement payload follows up to FIN (KR 2.5, see `peers.rs`).
const STREAM_MEDIA_ANNOUNCE: u8 = 0x02;
/// First byte of a datagram.
const DATAGRAM_UNRELIABLE: u8 = 0x00;
const DATAGRAM_UNRELIABLE_ORDERED: u8 = 0x01;
/// Media datagram: `[0x02][track number][marshaled RTP packet]`.
const DATAGRAM_MEDIA: u8 = 0x02;
/// Largest media announcement accepted.
const MAX_MEDIA_ANNOUNCE: usize = 4096;
/// Media items queued for the consumer; media datagrams beyond it are
/// dropped (late audio is useless), announcements wait.
const MEDIA_CAPACITY: usize = 1024;
const ORDERED_FRAME_HEADER: usize = 4;

/// Received messages queued for the consumer. The readers wait when it is
/// full, so a slow consumer turns into QUIC flow control (streams) or
/// datagram drops in quinn's receive buffer, never unbounded memory.
const INCOMING_CAPACITY: usize = 1024;
/// Ordered frames queued for the stream writer.
const ORDERED_QUEUE: usize = 64;
/// How long teardown waits for CONNECTION_CLOSE to leave before ICE closes.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// One received message.
pub(crate) type Incoming = (DeliveryMethod, Bytes);

/// One received media item (RTP over QUIC DATAGRAM, KR 2.5).
#[derive(Debug)]
pub(crate) enum MediaIncoming {
    /// A track announcement (opaque to the link).
    Announce(Bytes),
    /// A marshaled RTP packet of the announced track number `track`.
    Packet { track: u8, rtp: Bytes },
}

type OrderedJob = (Bytes, oneshot::Sender<Result<(), IceQuicError>>);

/// A snapshot of quinn's connection statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct IceQuicStats {
    /// Smoothed RTT.
    pub rtt: Duration,
    /// Congestion window, bytes.
    pub cwnd: u64,
    pub sent_packets: u64,
    pub lost_packets: u64,
    pub lost_bytes: u64,
    pub congestion_events: u64,
    /// UDP payload bytes handed to / received from ICE (QUIC packets,
    /// excluding ICE's own STUN).
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// What [`super::connect`] hands over once QUIC and the ordered stream are
/// up.
pub(super) struct LinkParts {
    pub agent: Arc<Agent>,
    pub endpoint: quinn::Endpoint,
    pub conn: quinn::Connection,
    pub ordered_send: quinn::SendStream,
    pub ordered_recv: quinn::RecvStream,
    pub max_message_bytes: usize,
    pub state: watch::Sender<IceQuicState>,
    pub ice_state: watch::Receiver<IceConnectionState>,
}

/// Shared between the handle and the background tasks.
struct Shared {
    conn: quinn::Connection,
    endpoint: quinn::Endpoint,
    /// Taken by whichever teardown runs first.
    agent: StdMutex<Option<Arc<Agent>>>,
    state: watch::Sender<IceQuicState>,
}

impl Shared {
    /// Close the connection, recording `state` unless a terminal state was
    /// already recorded.
    fn close(&self, code: u32, state: IceQuicState, reason: &'static [u8]) {
        record_end(&self.state, state);
        self.conn.close(code.into(), reason);
    }

    fn protocol_violation(&self, what: &str) {
        tracing::warn!(what, "ICE+QUIC: peer violated the link protocol; closing");
        self.close(
            close_code::PROTOCOL_VIOLATION,
            IceQuicState::Failed,
            b"protocol violation",
        );
    }

    /// Let CONNECTION_CLOSE drain through ICE, then close the ICE agent.
    async fn teardown(&self) {
        let drained = self.endpoint.wait_idle();
        let _ = tokio::time::timeout(DRAIN_TIMEOUT, drained).await;
        let agent = self.agent.lock().expect("agent mutex poisoned").take();
        if let Some(agent) = agent {
            if let Err(error) = agent.close().await {
                tracing::debug!(%error, "ICE+QUIC: ICE agent close");
            }
        }
    }
}

/// Record the first terminal state; later ones (e.g. the connection-end
/// watcher after a local close) do not overwrite it.
fn record_end(state: &watch::Sender<IceQuicState>, end: IceQuicState) {
    state.send_if_modified(|current| {
        let open = matches!(current, IceQuicState::Connecting | IceQuicState::Connected);
        if open {
            *current = end;
        }
        open
    });
}

/// The state a connection end maps to, for an end we did not initiate.
fn terminal_state(error: &quinn::ConnectionError) -> IceQuicState {
    match error {
        quinn::ConnectionError::ApplicationClosed(close)
            if close.error_code == close_code::NORMAL.into() =>
        {
            IceQuicState::Disconnected
        }
        quinn::ConnectionError::LocallyClosed => IceQuicState::Disconnected,
        _ => IceQuicState::Failed,
    }
}

/// A connected ICE+QUIC link.
///
/// Dropping it closes the connection normally (the peer sees
/// `Disconnected`); [`Self::close`] does the same and also waits for the
/// close to drain and ICE to shut down.
pub(crate) struct IceQuicLink {
    shared: Arc<Shared>,
    ordered: mpsc::Sender<OrderedJob>,
    ordered_seq: AtomicU16,
    max_message_bytes: usize,
    incoming: StdMutex<Option<mpsc::Receiver<Incoming>>>,
    media: StdMutex<Option<mpsc::Receiver<MediaIncoming>>>,
}

impl IceQuicLink {
    pub(super) fn start(parts: LinkParts) -> Self {
        let LinkParts {
            agent,
            endpoint,
            conn,
            ordered_send,
            ordered_recv,
            max_message_bytes,
            state,
            mut ice_state,
        } = parts;
        state.send_replace(IceQuicState::Connected);
        let shared = Arc::new(Shared {
            conn: conn.clone(),
            endpoint,
            agent: StdMutex::new(Some(agent)),
            state,
        });
        let (incoming_tx, incoming_rx) = mpsc::channel(INCOMING_CAPACITY);
        let (media_tx, media_rx) = mpsc::channel(MEDIA_CAPACITY);

        // Connection end: record the state, then release ICE.
        let watcher = shared.clone();
        tokio::spawn(async move {
            let error = watcher.conn.closed().await;
            tracing::debug!(%error, "ICE+QUIC: connection closed");
            record_end(&watcher.state, terminal_state(&error));
            watcher.teardown().await;
        });

        // ICE failure under a live connection (backstop; QUIC idle timeout
        // normally fires first).
        let ice_watch = shared.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = ice_state.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        if *ice_state.borrow_and_update() == IceConnectionState::Failed {
                            ice_watch.close(
                                close_code::ICE_FAILED,
                                IceQuicState::Failed,
                                b"ICE failed",
                            );
                            return;
                        }
                    }
                    _ = ice_watch.conn.closed() => return,
                }
            }
        });

        tokio::spawn(read_ordered(
            shared.clone(),
            ordered_recv,
            max_message_bytes,
            incoming_tx.clone(),
        ));
        tokio::spawn(accept_messages(
            shared.clone(),
            max_message_bytes,
            incoming_tx.clone(),
            media_tx.clone(),
        ));
        tokio::spawn(read_datagrams(shared.clone(), incoming_tx, media_tx));

        let (ordered, jobs) = mpsc::channel(ORDERED_QUEUE);
        tokio::spawn(write_ordered(ordered_send, jobs));

        Self {
            shared,
            ordered,
            ordered_seq: AtomicU16::new(0),
            max_message_bytes,
            incoming: StdMutex::new(Some(incoming_rx)),
            media: StdMutex::new(Some(media_rx)),
        }
    }

    /// The media receive side (announcements and RTP datagrams). Returns
    /// `None` after the first call.
    pub(crate) fn take_media_receiver(&self) -> Option<mpsc::Receiver<MediaIncoming>> {
        self.media.lock().expect("media mutex poisoned").take()
    }

    /// Send a media track announcement on its own unidirectional stream
    /// (reliable; the peer needs it to interpret the track's datagrams).
    pub(crate) async fn send_media_announce(&self, data: Bytes) -> Result<(), IceQuicError> {
        if data.len() > MAX_MEDIA_ANNOUNCE {
            return Err(IceQuicError::TooLarge {
                method: DeliveryMethod::ReliableUnordered,
                size: data.len(),
                limit: MAX_MEDIA_ANNOUNCE,
            });
        }
        let conn = self.shared.conn.clone();
        tokio::spawn(async move {
            let mut stream = conn.open_uni().await?;
            stream
                .write_all_chunks(&mut [Bytes::from_static(&[STREAM_MEDIA_ANNOUNCE]), data])
                .await?;
            stream.finish().map_err(|_| IceQuicError::Closed)
        })
        .await
        .map_err(|_| IceQuicError::Closed)?
    }

    /// Send one marshaled RTP packet of track number `track` as a QUIC
    /// DATAGRAM. Never waits; a packet larger than the datagram limit is an
    /// error (RTP is packetized to fit the path MTU by its sender).
    pub(crate) fn send_media(&self, track: u8, rtp: Bytes) -> Result<(), IceQuicError> {
        self.send_datagram(DeliveryMethod::Unreliable, &[DATAGRAM_MEDIA, track], rtp)
    }

    /// The receive side. Yields every message the peer sent, tagged with the
    /// method it was sent with, and ends (`None`) when the connection ends.
    /// Returns `None` after the first call.
    pub(crate) fn take_receiver(&self) -> Option<mpsc::Receiver<Incoming>> {
        let mut incoming = self.incoming.lock().expect("incoming mutex poisoned");
        incoming.take()
    }

    /// Send one message.
    ///
    /// Reliable methods return once the message is in quinn's send buffer
    /// (they wait for flow-control credit). Unreliable methods never wait.
    /// Cancelling the future never leaves a partial message on the wire: an
    /// ordered frame is either queued whole or not at all, and a per-message
    /// stream is written by its own task.
    pub(crate) async fn send(
        &self,
        method: DeliveryMethod,
        data: Bytes,
    ) -> Result<(), IceQuicError> {
        if let Some(error) = self.shared.conn.close_reason() {
            return Err(IceQuicError::Connection(error));
        }
        match method {
            DeliveryMethod::ReliableOrdered => {
                self.check_reliable(method, data.len())?;
                let (reply, done) = oneshot::channel();
                self.ordered
                    .reserve()
                    .await
                    .map_err(|_| self.closed_error())?
                    .send((data, reply));
                done.await.map_err(|_| self.closed_error())?
            }
            DeliveryMethod::ReliableUnordered => {
                self.check_reliable(method, data.len())?;
                let conn = self.shared.conn.clone();
                tokio::spawn(async move {
                    let mut stream = conn.open_uni().await?;
                    stream
                        .write_all_chunks(&mut [Bytes::from_static(&[STREAM_MESSAGE]), data])
                        .await?;
                    stream.finish().map_err(|_| IceQuicError::Closed)
                })
                .await
                .map_err(|_| IceQuicError::Closed)?
            }
            DeliveryMethod::Unreliable => self.send_datagram(method, &[DATAGRAM_UNRELIABLE], data),
            DeliveryMethod::UnreliableOrdered => {
                let seq = self.ordered_seq.fetch_add(1, Ordering::Relaxed);
                let [hi, lo] = seq.to_be_bytes();
                self.send_datagram(method, &[DATAGRAM_UNRELIABLE_ORDERED, hi, lo], data)
            }
        }
    }

    fn check_reliable(&self, method: DeliveryMethod, size: usize) -> Result<(), IceQuicError> {
        if size > self.max_message_bytes {
            return Err(IceQuicError::TooLarge {
                method,
                size,
                limit: self.max_message_bytes,
            });
        }
        Ok(())
    }

    fn closed_error(&self) -> IceQuicError {
        self.shared
            .conn
            .close_reason()
            .map_or(IceQuicError::Closed, IceQuicError::Connection)
    }

    fn send_datagram(
        &self,
        method: DeliveryMethod,
        header: &[u8],
        data: Bytes,
    ) -> Result<(), IceQuicError> {
        let max = self
            .shared
            .conn
            .max_datagram_size()
            .ok_or(IceQuicError::DatagramUnsupported)?;
        let limit = max.saturating_sub(header.len());
        if data.len() > limit {
            return Err(IceQuicError::TooLarge {
                method,
                size: data.len(),
                limit,
            });
        }
        let mut datagram = BytesMut::with_capacity(header.len() + data.len());
        datagram.put_slice(header);
        datagram.put_slice(&data);
        self.shared
            .conn
            .send_datagram(datagram.freeze())
            .map_err(|error| match error {
                quinn::SendDatagramError::ConnectionLost(error) => IceQuicError::Connection(error),
                quinn::SendDatagramError::TooLarge => IceQuicError::TooLarge {
                    method,
                    size: data.len(),
                    limit,
                },
                _ => IceQuicError::DatagramUnsupported,
            })
    }

    /// Largest payload [`Self::send`] accepts for `method`. Reliable methods:
    /// the negotiated `min(local, remote)` `max_message_bytes`. Datagram
    /// methods: the current QUIC DATAGRAM limit minus this link's header
    /// (`None` if the peer does not support DATAGRAM). The datagram limit
    /// follows the path MTU (fixed at the 1200-byte initial MTU while PMTU
    /// discovery is off).
    pub(crate) fn message_size_limit(&self, method: DeliveryMethod) -> Option<usize> {
        match method {
            DeliveryMethod::ReliableOrdered | DeliveryMethod::ReliableUnordered => {
                Some(self.max_message_bytes)
            }
            DeliveryMethod::Unreliable | DeliveryMethod::UnreliableOrdered => {
                self.max_datagram_payload(method)
            }
        }
    }

    /// See [`Self::message_size_limit`]; `None` for reliable methods.
    pub(crate) fn max_datagram_payload(&self, method: DeliveryMethod) -> Option<usize> {
        let header = match method {
            DeliveryMethod::Unreliable => 1,
            DeliveryMethod::UnreliableOrdered => 3,
            _ => return None,
        };
        self.shared
            .conn
            .max_datagram_size()
            .map(|max| max.saturating_sub(header))
    }

    /// Connection state. `Connected` while the link is up, then
    /// `Disconnected` (normal close by either side) or `Failed`.
    pub(crate) fn state(&self) -> watch::Receiver<IceQuicState> {
        self.shared.state.subscribe()
    }

    /// Why the connection ended, once it has.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn close_reason(&self) -> Option<quinn::ConnectionError> {
        self.shared.conn.close_reason()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn stats(&self) -> IceQuicStats {
        let stats = self.shared.conn.stats();
        IceQuicStats {
            rtt: stats.path.rtt,
            cwnd: stats.path.cwnd,
            sent_packets: stats.path.sent_packets,
            lost_packets: stats.path.lost_packets,
            lost_bytes: stats.path.lost_bytes,
            congestion_events: stats.path.congestion_events,
            bytes_sent: stats.udp_tx.bytes,
            bytes_received: stats.udp_rx.bytes,
        }
    }

    /// Close normally and wait (bounded) for the close to reach the peer and
    /// ICE to shut down. Idempotent.
    pub(crate) async fn close(&self) {
        self.shared
            .close(close_code::NORMAL, IceQuicState::Disconnected, b"closed");
        self.shared.teardown().await;
    }
}

impl Drop for IceQuicLink {
    fn drop(&mut self) {
        // The connection-end watcher task finishes the teardown.
        self.shared
            .close(close_code::NORMAL, IceQuicState::Disconnected, b"closed");
    }
}

/// Writes ordered frames in queue order. A failed write ends the stream;
/// later jobs see the channel closed.
async fn write_ordered(mut stream: quinn::SendStream, mut jobs: mpsc::Receiver<OrderedJob>) {
    while let Some((data, reply)) = jobs.recv().await {
        // check_reliable bounds the size by a u32 limit.
        let header = Bytes::copy_from_slice(&(data.len() as u32).to_be_bytes());
        let result = stream.write_all_chunks(&mut [header, data]).await;
        let failed = result.is_err();
        let _ = reply.send(result.map_err(IceQuicError::from));
        if failed {
            return;
        }
    }
}

async fn read_ordered(
    shared: Arc<Shared>,
    mut stream: quinn::RecvStream,
    max: usize,
    tx: mpsc::Sender<Incoming>,
) {
    loop {
        let mut header = [0; ORDERED_FRAME_HEADER];
        if let Err(error) = stream.read_exact(&mut header).await {
            return ordered_read_failed(&shared, error);
        }
        let len = u32::from_be_bytes(header) as usize;
        if len > max {
            return shared.protocol_violation("ordered frame above maxMessageBytes");
        }
        let mut data = vec![0; len];
        if let Err(error) = stream.read_exact(&mut data).await {
            return ordered_read_failed(&shared, error);
        }
        // A dropped receiver means nobody consumes; keep draining.
        let message = (DeliveryMethod::ReliableOrdered, Bytes::from(data));
        let _ = tx.send(message).await;
    }
}

fn ordered_read_failed(shared: &Shared, error: quinn::ReadExactError) {
    match error {
        quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(_)) => {}
        other => {
            tracing::debug!(error = %other, "ICE+QUIC: ordered stream ended");
            shared.protocol_violation("ordered stream ended or was reset");
        }
    }
}

async fn accept_messages(
    shared: Arc<Shared>,
    max: usize,
    tx: mpsc::Sender<Incoming>,
    media: mpsc::Sender<MediaIncoming>,
) {
    let mut readers = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = shared.conn.accept_uni() => {
                let Ok(stream) = accepted else { return };
                readers.spawn(read_message(shared.clone(), stream, max, tx.clone(), media.clone()));
            }
            Some(_) = readers.join_next(), if !readers.is_empty() => {}
        }
    }
}

/// One per-message stream: the tag byte, then the payload up to FIN.
/// Streams are read concurrently; a message never waits for another's FIN.
async fn read_message(
    shared: Arc<Shared>,
    mut stream: quinn::RecvStream,
    max: usize,
    tx: mpsc::Sender<Incoming>,
    media: mpsc::Sender<MediaIncoming>,
) {
    let mut tag = [0];
    match stream.read_exact(&mut tag).await {
        Ok(()) if tag[0] == STREAM_MESSAGE => {}
        Ok(()) if tag[0] == STREAM_MEDIA_ANNOUNCE => {
            match stream.read_to_end(MAX_MEDIA_ANNOUNCE).await {
                Ok(data) => {
                    let _ = media.send(MediaIncoming::Announce(Bytes::from(data))).await;
                }
                Err(quinn::ReadToEndError::TooLong) => {
                    shared.protocol_violation("media announcement too long");
                }
                Err(error) => tracing::debug!(%error, "ICE+QUIC: media announcement failed"),
            }
            return;
        }
        Ok(()) => return shared.protocol_violation("unknown stream tag"),
        Err(quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(_))) => return,
        Err(error) => {
            tracing::debug!(%error, "ICE+QUIC: message stream ended before its tag");
            return;
        }
    }
    match stream.read_to_end(max).await {
        Ok(data) => {
            let message = (DeliveryMethod::ReliableUnordered, Bytes::from(data));
            let _ = tx.send(message).await;
        }
        Err(quinn::ReadToEndError::TooLong) => {
            shared.protocol_violation("message stream above maxMessageBytes");
        }
        Err(error) => {
            // Reset by the sender or connection loss: no message.
            tracing::debug!(%error, "ICE+QUIC: message stream failed");
        }
    }
}

async fn read_datagrams(
    shared: Arc<Shared>,
    tx: mpsc::Sender<Incoming>,
    media: mpsc::Sender<MediaIncoming>,
) {
    // Newest UnreliableOrdered sequence delivered.
    let mut newest: Option<u16> = None;
    while let Ok(datagram) = shared.conn.read_datagram().await {
        let message = match datagram.first() {
            Some(&DATAGRAM_MEDIA) if datagram.len() >= 2 => {
                let packet = MediaIncoming::Packet {
                    track: datagram[1],
                    rtp: datagram.slice(2..),
                };
                // Never block the datagram reader on media.
                let _ = media.try_send(packet);
                continue;
            }
            Some(&DATAGRAM_UNRELIABLE) => (DeliveryMethod::Unreliable, datagram.slice(1..)),
            Some(&DATAGRAM_UNRELIABLE_ORDERED) if datagram.len() >= 3 => {
                let seq = u16::from_be_bytes([datagram[1], datagram[2]]);
                // RFC 1982 serial comparison: deliver only datagrams newer
                // than the newest delivered; drop late and duplicate ones.
                if newest.is_some_and(|n| (seq.wrapping_sub(n) as i16) <= 0) {
                    continue;
                }
                newest = Some(seq);
                (DeliveryMethod::UnreliableOrdered, datagram.slice(3..))
            }
            _ => {
                let len = datagram.len();
                tracing::debug!(len, "ICE+QUIC: dropping malformed datagram");
                continue;
            }
        };
        let _ = tx.send(message).await;
    }
}
