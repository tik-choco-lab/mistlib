//! Per-room ICE+QUIC peer management for the selection layer (SPEC-25 O2.2).
//!
//! [`IceQuicPeers`] owns every ICE+QUIC link of one room: setup (the lower
//! node ID offers, as for WebRTC), the per-peer send queue and writer task,
//! delivery of inbound messages to the room's [`NetworkEventHandler`],
//! connection events, and the fallback to WebRTC.
//!
//! Rules:
//! - Only a peer whose link is `Connected` is carried by ICE+QUIC; while an
//!   attempt is still connecting, sends keep going to WebRTC (which then has
//!   no session either, exactly as during a WebRTC attempt).
//! - Any setup failure (gathering, missing answer, ICE, TLS pin, timeout)
//!   pins the pair to WebRTC for [`FALLBACK_PIN`] and immediately starts a
//!   WebRTC connection to it. An established link that later fails is torn
//!   down normally; the next connect chooses again.
//! - The room-wide connection limit counts ICE+QUIC peers together with
//!   WebRTC peers: WebRTC adds [`IceQuicPeers::active_counter`] to its own
//!   count, and ICE+QUIC checks the WebRTC count before starting.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use mistlib_core::action::OverlayAction;
use mistlib_core::error::{MistError, Result};
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::transport::send::{closed_send, SendFuture, SendReservation};
use mistlib_core::transport::{NetworkEvent, NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use tokio::sync::{mpsc, oneshot};

use super::link::MediaIncoming;
use super::{
    IceQuicAnswer, IceQuicAnswerer, IceQuicConfig, IceQuicError, IceQuicLink, IceQuicOffer,
    IceQuicOfferer, IceQuicState,
};
use crate::transports::media_packet::{self, MediaPacketEvent, TrackDescriptor};
use crate::transports::webrtc::{SendCommit, WebRtcTransport};

/// How long a pair whose ICE+QUIC setup failed stays on WebRTC.
pub(crate) const FALLBACK_PIN: Duration = Duration::from_secs(600);
/// How long the offerer waits for the iq1 answer before falling back.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(8);
/// When the offerer resends an unanswered offer (same payload), as WebRTC
/// resends its Offer: a lost offer costs ~1.5 s instead of a fallback.
const OFFER_RESEND_AFTER: [Duration; 2] =
    [Duration::from_millis(1500), Duration::from_millis(3000)];
/// Per-peer send queue depth (messages).
const SEND_QUEUE: usize = 1024;
/// Media datagrams kept per not-yet-announced track number.
const EARLY_MEDIA_PACKETS: usize = 16;

struct Queued {
    data: Bytes,
    method: DeliveryMethod,
    done: Option<oneshot::Sender<Result<()>>>,
}

struct Peer {
    attempt: u64,
    state: ConnectionState,
    tx: Option<mpsc::Sender<Queued>>,
    link: Option<Arc<IceQuicLink>>,
    /// Answerer side: the ICE ufrag of the offer this attempt answers, and
    /// the answer sent for it, so a resent offer gets the same answer.
    offer_ufrag: Option<String>,
    answer: Option<String>,
}

pub(crate) struct IceQuicPeers {
    local: NodeId,
    room_id: String,
    signaler: Arc<dyn Signaler>,
    config: IceQuicConfig,
    webrtc: Weak<WebRtcTransport>,
    max_connections: Arc<dyn Fn() -> usize + Send + Sync>,
    handler: Mutex<Option<Arc<dyn NetworkEventHandler>>>,
    peers: RwLock<HashMap<NodeId, Peer>>,
    answers: Mutex<HashMap<NodeId, (u64, oneshot::Sender<IceQuicAnswer>)>>,
    pinned: Mutex<HashMap<NodeId, Instant>>,
    retained_tails: Mutex<HashMap<NodeId, oneshot::Receiver<()>>>,
    /// Per peer: local track id -> announced track number.
    media_out: Mutex<HashMap<NodeId, HashMap<String, u8>>>,
    active: Arc<AtomicUsize>,
    next_attempt: AtomicU64,
}

fn link_error(error: IceQuicError) -> MistError {
    match error {
        IceQuicError::TooLarge { size, limit, .. } => MistError::MessageTooLarge {
            size,
            limit: u32::try_from(limit).unwrap_or(u32::MAX),
        },
        other => MistError::Network(format!("ice-quic: {other}")),
    }
}

impl IceQuicPeers {
    pub(crate) fn new(
        local: NodeId,
        room_id: String,
        signaler: Arc<dyn Signaler>,
        config: IceQuicConfig,
        webrtc: &Arc<WebRtcTransport>,
    ) -> Arc<Self> {
        let max_webrtc = Arc::downgrade(webrtc);
        Arc::new(Self {
            local,
            room_id,
            signaler,
            config,
            webrtc: Arc::downgrade(webrtc),
            max_connections: Arc::new(move || {
                max_webrtc
                    .upgrade()
                    .map(|wt| wt.max_connections.load(Ordering::Relaxed) as usize)
                    .unwrap_or(0)
            }),
            handler: Mutex::new(None),
            peers: RwLock::new(HashMap::new()),
            answers: Mutex::new(HashMap::new()),
            pinned: Mutex::new(HashMap::new()),
            retained_tails: Mutex::new(HashMap::new()),
            media_out: Mutex::new(HashMap::new()),
            active: Arc::new(AtomicUsize::new(0)),
            next_attempt: AtomicU64::new(1),
        })
    }

    /// Number of ICE+QUIC peers that are connecting or connected; WebRTC
    /// adds it to its own count when enforcing the connection limit.
    pub(crate) fn active_counter(&self) -> Arc<AtomicUsize> {
        self.active.clone()
    }

    pub(crate) fn set_handler(&self, handler: Arc<dyn NetworkEventHandler>) {
        *self.handler.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }

    // ---- state ----

    pub(crate) fn pinned_to_webrtc(&self, node: &NodeId) -> bool {
        let mut pinned = self.pinned.lock().unwrap_or_else(|e| e.into_inner());
        match pinned.get(node) {
            Some(until) if Instant::now() < *until => true,
            Some(_) => {
                pinned.remove(node);
                false
            }
            None => false,
        }
    }

    fn pin(&self, node: &NodeId) {
        self.pinned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(node.clone(), Instant::now() + FALLBACK_PIN);
    }

    /// The state of `node`'s ICE+QUIC attempt or link, if there is one.
    pub(crate) fn state(&self, node: &NodeId) -> Option<ConnectionState> {
        self.peers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(node)
            .map(|peer| peer.state)
    }

    /// Whether `node` is carried by an established ICE+QUIC link.
    pub(crate) fn carries(&self, node: &NodeId) -> bool {
        self.state(node) == Some(ConnectionState::Connected)
    }

    pub(crate) fn active_states(&self) -> Vec<(NodeId, ConnectionState)> {
        self.peers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(node, peer)| (node.clone(), peer.state))
            .collect()
    }

    pub(crate) fn connected_nodes(&self) -> Vec<NodeId> {
        self.peers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, peer)| peer.state == ConnectionState::Connected)
            .map(|(node, _)| node.clone())
            .collect()
    }

    pub(crate) fn message_size_limit(&self, node: &NodeId) -> Option<u32> {
        let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
        let link = peers.get(node)?.link.as_ref()?;
        link.message_size_limit(DeliveryMethod::ReliableOrdered)
            .map(|limit| u32::try_from(limit).unwrap_or(u32::MAX))
    }

    // ---- setup ----

    /// Reserve a slot for a new attempt, enforcing the shared connection
    /// limit. `None` when `node` already has an attempt/link or the room is
    /// at its limit.
    fn begin(&self, node: &NodeId, replace: bool) -> Option<u64> {
        let webrtc_active = self
            .webrtc
            .upgrade()
            .map(|wt| wt.get_active_connection_states().len())
            .unwrap_or(0);
        let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = peers.get(node) {
            if !replace {
                return None;
            }
            if let Some(link) = existing.link.clone() {
                tokio::spawn(async move { link.close().await });
            }
            let was_connected = existing.state == ConnectionState::Connected;
            peers.remove(node);
            self.active.fetch_sub(1, Ordering::Relaxed);
            if was_connected {
                crate::events::on_disconnected_internal(self.room_id.clone(), node.clone());
            }
        }
        if webrtc_active + peers.len() >= (self.max_connections)() {
            return None;
        }
        let attempt = self.next_attempt.fetch_add(1, Ordering::Relaxed);
        peers.insert(
            node.clone(),
            Peer {
                attempt,
                state: ConnectionState::Connecting,
                tx: None,
                link: None,
                offer_ufrag: None,
                answer: None,
            },
        );
        self.active.fetch_add(1, Ordering::Relaxed);
        Some(attempt)
    }

    async fn send_signal(&self, node: &NodeId, kind: SignalingType, data: String) -> Result<()> {
        self.signaler
            .send_signaling(
                node,
                MessageContent::Data(SignalingData {
                    sender_id: self.local.clone(),
                    receiver_id: node.clone(),
                    room_id: self.room_id.clone(),
                    data,
                    signaling_type: kind,
                }),
            )
            .await
    }

    /// Start (or nudge) an ICE+QUIC connection to `node`. As for WebRTC, the
    /// lower node ID offers; the higher one sends a CONNECT `Request`
    /// (carrying its capabilities) so the lower one offers.
    pub(crate) async fn connect(self: &Arc<Self>, node: &NodeId) -> Result<()> {
        if self.local.0 < node.0 {
            if let Some(attempt) = self.begin(node, false) {
                let this = self.clone();
                let node = node.clone();
                tokio::spawn(async move { this.run_offer(node, attempt).await });
            }
            Ok(())
        } else {
            self.send_signal(
                node,
                SignalingType::Request,
                crate::transports::selector::connect_request_data(),
            )
            .await
        }
    }

    async fn run_offer(self: Arc<Self>, node: NodeId, attempt: u64) {
        let result = async {
            let offerer = IceQuicOfferer::gather(self.config.clone()).await?;
            let payload = serde_json::to_string(&offerer.offer())
                .map_err(|e| IceQuicError::Signal(e.to_string()))?;
            let (tx, rx) = oneshot::channel();
            self.answers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(node.clone(), (attempt, tx));
            self.send_signal(&node, SignalingType::IceQuicOffer, payload.clone())
                .await
                .map_err(|e| IceQuicError::Signal(e.to_string()))?;
            let sent_at = tokio::time::Instant::now();
            let deadline = sent_at + ANSWER_TIMEOUT;
            let mut resends = OFFER_RESEND_AFTER.iter().map(|after| sent_at + *after);
            let mut next_resend = resends.next();
            tokio::pin!(rx);
            let answer = loop {
                let wake = next_resend.unwrap_or(deadline).min(deadline);
                tokio::select! {
                    answer = &mut rx => break answer.map_err(|_| IceQuicError::Closed)?,
                    _ = tokio::time::sleep_until(wake) => {
                        if tokio::time::Instant::now() >= deadline {
                            return Err(IceQuicError::Timeout(ANSWER_TIMEOUT));
                        }
                        tracing::debug!("[LinkSelector] action=resend_offer peer={}", node.0);
                        let _ = self
                            .send_signal(&node, SignalingType::IceQuicOffer, payload.clone())
                            .await;
                        next_resend = resends.next();
                    }
                }
            };
            offerer.connect(answer).await
        }
        .await;
        self.answers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&node);
        match result {
            Ok(link) => self.install(&node, attempt, link),
            Err(error) => self.fail(&node, attempt, &error.to_string()),
        }
    }

    /// An iq1 offer from `from` (we are the higher node ID).
    pub(crate) fn handle_offer(self: &Arc<Self>, from: &NodeId, data: &str) {
        if self.pinned_to_webrtc(from) {
            tracing::debug!(
                "[LinkSelector] action=ignore_offer peer={} reason=pinned",
                from.0
            );
            return;
        }
        let offer: IceQuicOffer = match serde_json::from_str(data) {
            Ok(offer) => offer,
            Err(error) => {
                tracing::warn!(
                    "[LinkSelector] invalid iq1 offer from {}: {}",
                    from.0,
                    error
                );
                return;
            }
        };
        // A resent copy of the offer we are already answering: resend the
        // same answer (or wait until it exists) instead of restarting.
        let ufrag = offer.0.ice_ufrag.clone();
        let duplicate = {
            let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
            peers
                .get(from)
                .filter(|peer| {
                    peer.state == ConnectionState::Connecting
                        && peer.offer_ufrag.as_deref() == Some(ufrag.as_str())
                })
                .map(|peer| peer.answer.clone())
        };
        if let Some(answer) = duplicate {
            if let Some(answer) = answer {
                let this = self.clone();
                let from = from.clone();
                tokio::spawn(async move {
                    let _ = this
                        .send_signal(&from, SignalingType::IceQuicAnswer, answer)
                        .await;
                });
            }
            return;
        }
        // A new offer supersedes whatever we had for this peer (it restarted
        // or retried).
        let Some(attempt) = self.begin(from, true) else {
            tracing::info!(
                "[LinkSelector] action=reject_offer peer={} reason=limit",
                from.0
            );
            return;
        };
        if let Some(peer) = self
            .peers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(from)
            .filter(|peer| peer.attempt == attempt)
        {
            peer.offer_ufrag = Some(ufrag);
        }
        let this = self.clone();
        let from = from.clone();
        tokio::spawn(async move {
            let result = async {
                let answerer = IceQuicAnswerer::gather(this.config.clone(), offer).await?;
                let payload = serde_json::to_string(&answerer.answer())
                    .map_err(|e| IceQuicError::Signal(e.to_string()))?;
                if let Some(peer) = this
                    .peers
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&from)
                    .filter(|peer| peer.attempt == attempt)
                {
                    peer.answer = Some(payload.clone());
                }
                this.send_signal(&from, SignalingType::IceQuicAnswer, payload)
                    .await
                    .map_err(|e| IceQuicError::Signal(e.to_string()))?;
                answerer.connect().await
            }
            .await;
            match result {
                Ok(link) => this.install(&from, attempt, link),
                Err(error) => this.fail(&from, attempt, &error.to_string()),
            }
        });
    }

    /// An iq1 answer from `from` (we offered).
    pub(crate) fn handle_answer(&self, from: &NodeId, data: &str) {
        let answer: IceQuicAnswer = match serde_json::from_str(data) {
            Ok(answer) => answer,
            Err(error) => {
                tracing::warn!(
                    "[LinkSelector] invalid iq1 answer from {}: {}",
                    from.0,
                    error
                );
                return;
            }
        };
        let waiter = self
            .answers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(from);
        if let Some((_, tx)) = waiter {
            let _ = tx.send(answer);
        }
    }

    /// A WebRTC negotiation arrived from `node`: the peer chose WebRTC, so
    /// abandon a pending ICE+QUIC attempt and keep the pair on WebRTC.
    pub(crate) fn abandon_pending(&self, node: &NodeId) {
        let removed = {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            match peers.get(node) {
                Some(peer) if peer.state == ConnectionState::Connecting => peers.remove(node),
                _ => None,
            }
        };
        if removed.is_some() {
            self.active.fetch_sub(1, Ordering::Relaxed);
            self.answers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(node);
            self.pin(node);
            tracing::info!(
                "[LinkSelector] action=fallback peer={} reason=remote_chose_webrtc",
                node.0
            );
        }
    }

    fn install(self: &Arc<Self>, node: &NodeId, attempt: u64, link: IceQuicLink) {
        let link = Arc::new(link);
        let (tx, mut rx) = mpsc::channel::<Queued>(SEND_QUEUE);
        {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            match peers.get_mut(node) {
                Some(peer) if peer.attempt == attempt => {
                    peer.state = ConnectionState::Connected;
                    peer.tx = Some(tx);
                    peer.link = Some(link.clone());
                }
                _ => {
                    drop(peers);
                    tokio::spawn(async move { link.close().await });
                    return;
                }
            }
        }
        // Writer: one task per peer keeps per-peer submission order.
        let writer_link = link.clone();
        tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                let result = send_on(&writer_link, item.method, item.data).await;
                if let Some(done) = item.done {
                    let _ = done.send(result);
                }
            }
        });
        // Reader: deliver inbound messages like WebRTC DataChannel messages.
        if let Some(mut incoming) = link.take_receiver() {
            let handler = self
                .handler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let from = node.clone();
            tokio::spawn(async move {
                while let Some((_, data)) = incoming.recv().await {
                    if let Some(handler) = &handler {
                        handler.on_event(NetworkEvent {
                            from: from.clone(),
                            data,
                        });
                    }
                }
            });
        }
        // Media: RTP over DATAGRAM, labelled by the peer's announcements.
        if let Some(mut media) = link.take_media_receiver() {
            let room_id = self.room_id.clone();
            let from = node.clone();
            tokio::spawn(async move {
                let mut tracks: HashMap<u8, TrackDescriptor> = HashMap::new();
                // Datagrams can overtake the track's announcement stream; keep
                // a few per unknown track number and deliver them on announce.
                let mut early: HashMap<u8, Vec<Bytes>> = HashMap::new();
                let emit = |desc: &TrackDescriptor, rtp: Bytes| {
                    let mut buf = rtp;
                    let Ok(packet) =
                        <webrtc::rtp::packet::Packet as webrtc::util::Unmarshal>::unmarshal(
                            &mut buf,
                        )
                    else {
                        return;
                    };
                    media_packet::dispatch(MediaPacketEvent {
                        room_id: room_id.clone(),
                        remote_id: from.clone(),
                        track_id: desc.id.clone(),
                        stream_id: desc.stream_id.clone(),
                        kind: desc.kind.clone(),
                        mime_type: desc.mime_type.clone(),
                        clock_rate: desc.clock_rate,
                        channels: desc.channels,
                        packet,
                    });
                };
                while let Some(item) = media.recv().await {
                    match item {
                        MediaIncoming::Announce(data) => {
                            match serde_json::from_slice::<TrackDescriptor>(&data) {
                                Ok(desc) => {
                                    for rtp in early.remove(&desc.no).unwrap_or_default() {
                                        emit(&desc, rtp);
                                    }
                                    tracks.insert(desc.no, desc);
                                }
                                Err(error) => tracing::debug!(
                                    "[LinkSelector] invalid media announcement from {}: {}",
                                    from.0,
                                    error
                                ),
                            }
                        }
                        MediaIncoming::Packet { track, rtp } => match tracks.get(&track) {
                            Some(desc) => emit(desc, rtp),
                            None => {
                                let pending = early.entry(track).or_default();
                                if pending.len() < EARLY_MEDIA_PACKETS {
                                    pending.push(rtp);
                                }
                            }
                        },
                    }
                }
            });
        }
        // Watcher: tear down when the link ends.
        let this = self.clone();
        let watched = node.clone();
        let mut state = link.state();
        tokio::spawn(async move {
            loop {
                let current = *state.borrow_and_update();
                if matches!(current, IceQuicState::Disconnected | IceQuicState::Failed) {
                    break;
                }
                if state.changed().await.is_err() {
                    break;
                }
            }
            let reason = format!("{:?}", *state.borrow());
            this.teardown(&watched, attempt, &reason);
        });
        self.signaler.note_peer_connected(node);
        tracing::info!(
            "[LinkSelector] action=connected link=ice_quic peer={} total_ice_quic={}",
            node.0,
            self.connected_nodes().len()
        );
        crate::events::on_connected_internal(self.room_id.clone(), node.clone());
    }

    fn teardown(&self, node: &NodeId, attempt: u64, reason: &str) -> bool {
        let removed = {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            match peers.get(node) {
                Some(peer) if peer.attempt == attempt => peers.remove(node),
                _ => None,
            }
        };
        let Some(peer) = removed else {
            return false;
        };
        self.active.fetch_sub(1, Ordering::Relaxed);
        self.media_out
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(node);
        if let Some(link) = peer.link {
            tokio::spawn(async move { link.close().await });
        }
        if peer.state == ConnectionState::Connected {
            tracing::info!(
                "[LinkSelector] action=disconnected link=ice_quic peer={} reason={}",
                node.0,
                reason
            );
            crate::events::on_disconnected_internal(self.room_id.clone(), node.clone());
        }
        true
    }

    /// Setup failed: tear down, pin the pair to WebRTC and connect it there.
    fn fail(&self, node: &NodeId, attempt: u64, reason: &str) {
        if !self.teardown(node, attempt, reason) {
            return;
        }
        self.pin(node);
        tracing::info!(
            "[LinkSelector] action=fallback peer={} reason={}",
            node.0,
            reason.replace(char::is_whitespace, "_")
        );
        if let Some(webrtc) = self.webrtc.upgrade() {
            let node = node.clone();
            tokio::spawn(async move {
                let _ = <WebRtcTransport as Transport>::connect(&webrtc, &node).await;
            });
        }
    }

    pub(crate) fn disconnect(&self, node: &NodeId) {
        let attempt = self
            .peers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(node)
            .map(|peer| peer.attempt);
        if let Some(attempt) = attempt {
            self.teardown(node, attempt, "local_disconnect");
        }
    }

    pub(crate) fn close_all(&self) {
        let all: Vec<(NodeId, u64)> = self
            .peers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(node, peer)| (node.clone(), peer.attempt))
            .collect();
        for (node, attempt) in all {
            self.teardown(&node, attempt, "room_close_all");
        }
    }

    // ---- send admission (mirrors the WebRTC per-peer queue) ----

    fn sender(&self, node: &NodeId) -> Result<(mpsc::Sender<Queued>, u32)> {
        let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
        let peer = peers
            .get(node)
            .filter(|peer| peer.state == ConnectionState::Connected)
            .ok_or_else(|| MistError::RouteNotFound(node.clone()))?;
        let tx = peer.tx.clone().ok_or_else(closed_send)?;
        let limit = peer
            .link
            .as_ref()
            .and_then(|link| link.message_size_limit(DeliveryMethod::ReliableOrdered))
            .map(|limit| u32::try_from(limit).unwrap_or(u32::MAX))
            .unwrap_or(u32::MAX);
        Ok((tx, limit))
    }

    fn check(size: usize, limit: u32) -> Result<()> {
        if size > limit as usize {
            Err(MistError::MessageTooLarge { size, limit })
        } else {
            Ok(())
        }
    }

    pub(crate) async fn reserve_overlay_send(
        &self,
        node: &NodeId,
        _bytes: usize,
    ) -> Result<SendReservation> {
        let (tx, limit) = self.sender(node)?;
        let slot = tx.reserve_owned().await.map_err(|_| closed_send())?;
        let node = node.clone();
        Ok(Box::new(move |action| {
            let OverlayAction::SendMessage {
                to, data, method, ..
            } = action
            else {
                return Err(closed_send());
            };
            if to != node {
                return Err(closed_send());
            }
            Self::check(data.len(), limit)?;
            let (done, receipt) = oneshot::channel();
            slot.send(Queued {
                data,
                method,
                done: Some(done),
            });
            Ok(receipt)
        }))
    }

    pub(crate) async fn enqueue(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> Result<()> {
        let (tx, limit) = self.sender(node)?;
        Self::check(data.len(), limit)?;
        tx.send(Queued {
            data,
            method,
            done: None,
        })
        .await
        .map_err(|_| closed_send())
    }

    /// Like [`Self::enqueue`] but returns a `'static` future, chained after
    /// the previous retained send to the same peer so call order is kept.
    pub(crate) fn retain(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> SendFuture<'static, Result<()>> {
        let sender = self.sender(node);
        let (done, next) = oneshot::channel();
        let previous = self
            .retained_tails
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(node.clone(), next);
        Box::pin(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let result = async {
                let (tx, limit) = sender?;
                Self::check(data.len(), limit)?;
                tx.send(Queued {
                    data,
                    method,
                    done: None,
                })
                .await
                .map_err(|_| closed_send())
            }
            .await;
            let _ = done.send(());
            result
        })
    }

    pub(crate) fn try_enqueue(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> Result<()> {
        let (tx, limit) = self.sender(node)?;
        Self::check(data.len(), limit)?;
        tx.try_send(Queued {
            data,
            method,
            done: None,
        })
        .map_err(|_| MistError::Internal(format!("Send queue full or closed for {node}")))
    }

    pub(crate) fn try_enqueue_with(
        &self,
        node: &NodeId,
        build: impl FnOnce(&dyn Fn(usize) -> Result<()>, SendCommit<'_>) -> Result<()>,
    ) -> Result<()> {
        let (tx, limit) = self.sender(node)?;
        let permit = tx
            .try_reserve_owned()
            .map_err(|_| MistError::Internal(format!("Send queue full or closed for {node}")))?;
        build(
            &|size| Self::check(size, limit),
            Box::new(move |data, method| {
                permit.send(Queued {
                    data,
                    method,
                    done: None,
                });
                Ok(())
            }),
        )
    }

    pub(crate) async fn send_wait_with(
        &self,
        node: &NodeId,
        build: impl FnOnce(&dyn Fn(usize) -> Result<()>, SendCommit<'_>) -> Result<()>,
    ) -> Result<()> {
        let (tx, limit) = self.sender(node)?;
        let permit = tx.reserve_owned().await.map_err(|_| closed_send())?;
        let (done, receipt) = oneshot::channel();
        build(
            &|size| Self::check(size, limit),
            Box::new(move |data, method| {
                permit.send(Queued {
                    data,
                    method,
                    done: Some(done),
                });
                Ok(())
            }),
        )?;
        receipt.await.map_err(|_| closed_send())?
    }

    pub(crate) async fn send_wait(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> Result<()> {
        let (tx, limit) = self.sender(node)?;
        Self::check(data.len(), limit)?;
        let (done, receipt) = oneshot::channel();
        tx.send(Queued {
            data,
            method,
            done: Some(done),
        })
        .await
        .map_err(|_| closed_send())?;
        receipt.await.map_err(|_| closed_send())?
    }
}

impl IceQuicPeers {
    /// Send one marshaled RTP packet of `desc` to `node` over its established
    /// link, announcing the track to that peer first if needed. Silently
    /// skips a peer without an established link.
    pub(crate) async fn send_rtp(
        &self,
        node: &NodeId,
        desc: &TrackDescriptor,
        rtp: Bytes,
    ) -> Result<()> {
        let link = {
            let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
            match peers.get(node) {
                Some(peer) if peer.state == ConnectionState::Connected => peer.link.clone(),
                _ => None,
            }
        };
        let Some(link) = link else {
            return Ok(());
        };
        let (no, announce) = {
            let mut out = self.media_out.lock().unwrap_or_else(|e| e.into_inner());
            let tracks = out.entry(node.clone()).or_default();
            match tracks.get(&desc.id) {
                Some(no) => (*no, None),
                None => {
                    let no = u8::try_from(tracks.len()).map_err(|_| {
                        MistError::Internal("too many media tracks for one peer".into())
                    })?;
                    tracks.insert(desc.id.clone(), no);
                    let mut announced = desc.clone();
                    announced.no = no;
                    (no, Some(announced))
                }
            }
        };
        if let Some(announced) = announce {
            let data = serde_json::to_vec(&announced)
                .map_err(|e| MistError::Internal(format!("media announcement: {e}")))?;
            if let Err(error) = link.send_media_announce(Bytes::from(data)).await {
                // Forget it so the next packet retries the announcement.
                if let Some(tracks) = self
                    .media_out
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(node)
                {
                    tracks.remove(&desc.id);
                }
                return Err(link_error(error));
            }
        }
        link.send_media(no, rtp).map_err(link_error)
    }
}

/// Send one message; unreliable payloads that do not fit a datagram go on a
/// per-message reliable stream instead (best effort either way).
async fn send_on(link: &IceQuicLink, method: DeliveryMethod, data: Bytes) -> Result<()> {
    let method = match method {
        DeliveryMethod::Unreliable | DeliveryMethod::UnreliableOrdered
            if link
                .max_datagram_payload(method)
                .is_none_or(|max| data.len() > max) =>
        {
            DeliveryMethod::ReliableUnordered
        }
        other => other,
    };
    link.send(method, data).await.map_err(link_error)
}
