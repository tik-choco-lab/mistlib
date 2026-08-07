use async_trait::async_trait;
use mistlib_core::config::NostrSignalingConfig;
use mistlib_core::signaling::nostr::{
    random_subscription_id, DedupeCache, DiscoveryTable, InvitePskCrypto, NostrCodecConfig,
    TemporarySignalingIdentity,
};
use mistlib_core::signaling::{MessageContent, Signaler};
use mistlib_core::types::NodeId;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use web_time::{Duration, SystemTime, UNIX_EPOCH};

mod connection;
mod processing;
mod publish;
mod refresh;
mod relay_source;
mod resubscribe;

#[cfg(test)]
mod tests;

/// Subscription ids for a room's active discovery/message REQ filters.
///
/// These are generated once per room join and reused across relay
/// (re)connects and periodic filter refreshes so that re-issuing a REQ with
/// the same subscription id replaces the relay-side filter (NIP-01) instead
/// of opening an ever-growing set of parallel subscriptions.
#[derive(Clone)]
struct RoomSubscriptionIds {
    discovery: String,
    message: String,
}

impl RoomSubscriptionIds {
    fn generate() -> Self {
        Self {
            discovery: random_subscription_id(),
            message: random_subscription_id(),
        }
    }
}

#[derive(Clone)]
pub struct NostrSignaler {
    relays: Vec<String>,
    relay_list_url: Option<String>,
    local_node_id: NodeId,
    identity: TemporarySignalingIdentity,
    rotated_identity: Arc<Mutex<Option<TemporarySignalingIdentity>>>,
    session_epoch: Arc<AtomicU64>,
    codec_config: NostrCodecConfig,
    crypto: InvitePskCrypto,
    /// Serializes targeted (per-receiver) publishes end-to-end: held from
    /// sequence assignment through the `senders` mpsc enqueue in
    /// `publish_message_to_pubkey`. Without this, two concurrent targeted
    /// publishes to the same receiver can assign sequences in one order
    /// (T0: assign 5, T1: assign 6) but enqueue onto the relay channel in the
    /// other order (T1 enqueues 6, then T0 enqueues 5) because the
    /// CPU-bound crypto (ECDH + HKDF + AES-GCM + schnorr sign) between
    /// assignment and enqueue has no ordering guarantee across tasks. The
    /// receiver's monotonic sequence gate would then see 6 before 5 and
    /// silently discard 5 forever (no retransmit). Holding this mutex across
    /// both steps makes sequence-assignment order and wire-enqueue order the
    /// same order, at the cost of serializing outbound targeted publishes
    /// per node (acceptable: tens of messages/sec, ~100us of crypto each).
    /// Discovery (broadcast) publishes carry no sequence and do not take it.
    send_order: Arc<Mutex<()>>,
    senders: Arc<Mutex<Vec<mpsc::Sender<String>>>>,
    room_id: Arc<Mutex<Option<String>>>,
    discovery_table: Arc<Mutex<DiscoveryTable>>,
    dedupe: Arc<Mutex<DedupeCache>>,
    message_dedupe: Arc<Mutex<DedupeCache>>,
    outgoing_sequences: Arc<Mutex<HashMap<String, u64>>>,
    incoming_sequences: Arc<Mutex<HashMap<String, u64>>>,
    local_joined_at: Arc<Mutex<Option<u64>>>,
    requested_pubkeys: Arc<Mutex<HashSet<String>>>,
    peer_sessions: Arc<Mutex<HashMap<String, u64>>>,
    refresh_epoch: Arc<AtomicU64>,
    resubscribe_epoch: Arc<AtomicU64>,
    subscription_ids: Arc<Mutex<Option<RoomSubscriptionIds>>>,
    reconnect_cancel: Arc<Mutex<Option<CancellationToken>>>,
}

impl NostrSignaler {
    pub fn new(local_node_id: NodeId, config: NostrSignalingConfig) -> Self {
        let codec_config = NostrCodecConfig::from_config(&config);
        let crypto = InvitePskCrypto::new(&config.invite_salt, &config.invite_code);
        let dedupe_ttl = Duration::from_secs(config.ttl_seconds.saturating_mul(2).max(1));
        let relay_list_url = config.effective_relay_list_url().map(str::to_owned);
        Self {
            relays: config.relays,
            relay_list_url,
            local_node_id,
            identity: TemporarySignalingIdentity::generate(),
            rotated_identity: Arc::new(Mutex::new(None)),
            session_epoch: Arc::new(AtomicU64::new(0)),
            codec_config,
            crypto,
            send_order: Arc::new(Mutex::new(())),
            senders: Arc::new(Mutex::new(Vec::new())),
            room_id: Arc::new(Mutex::new(None)),
            discovery_table: Arc::new(Mutex::new(DiscoveryTable::default())),
            dedupe: Arc::new(Mutex::new(DedupeCache::new(dedupe_ttl))),
            message_dedupe: Arc::new(Mutex::new(DedupeCache::new(dedupe_ttl))),
            outgoing_sequences: Arc::new(Mutex::new(HashMap::new())),
            incoming_sequences: Arc::new(Mutex::new(HashMap::new())),
            local_joined_at: Arc::new(Mutex::new(None)),
            requested_pubkeys: Arc::new(Mutex::new(HashSet::new())),
            peer_sessions: Arc::new(Mutex::new(HashMap::new())),
            refresh_epoch: Arc::new(AtomicU64::new(0)),
            resubscribe_epoch: Arc::new(AtomicU64::new(0)),
            subscription_ids: Arc::new(Mutex::new(None)),
            reconnect_cancel: Arc::new(Mutex::new(None)),
        }
    }

    /// Returns the current room's discovery/message subscription ids,
    /// generating and persisting them on first use so that all relay
    /// connections and later filter refreshes reuse the same ids.
    async fn current_subscription_ids(&self) -> RoomSubscriptionIds {
        let mut lock = self.subscription_ids.lock().await;
        if let Some(ids) = lock.as_ref() {
            return ids.clone();
        }
        let ids = RoomSubscriptionIds::generate();
        *lock = Some(ids.clone());
        ids
    }

    async fn set_room_id(&self, room_id: &str) -> mistlib_core::error::Result<()> {
        if room_id.is_empty() {
            return Ok(());
        }
        let changed = {
            let mut lock = self.room_id.lock().await;
            if lock.as_deref() == Some(room_id) {
                false
            } else {
                *lock = Some(room_id.to_string());
                true
            }
        };
        if changed {
            self.discovery_table.lock().await.clear();
            self.dedupe.lock().await.clear();
            self.message_dedupe.lock().await.clear();
            self.outgoing_sequences.lock().await.clear();
            self.incoming_sequences.lock().await.clear();
            self.requested_pubkeys.lock().await.clear();
            self.peer_sessions.lock().await.clear();
            self.subscription_ids.lock().await.take();
            *self.local_joined_at.lock().await = Some(current_unix_millis());
            self.subscribe_room(room_id).await?;
            self.spawn_discovery_refresh(room_id.to_string());
            self.spawn_room_resubscribe(room_id.to_string());
        }
        Ok(())
    }

    async fn subscribe_room(&self, room_id: &str) -> mistlib_core::error::Result<()> {
        let mut senders = self.senders.lock().await;
        if senders.is_empty() {
            return Ok(());
        }

        let mut subscribed = 0_usize;
        let mut alive = Vec::with_capacity(senders.len());
        for tx in senders.drain(..) {
            if tx.is_closed() {
                tracing::warn!("NostrSignaler: dropping closed relay sender while subscribing");
                continue;
            }
            match self.subscribe(&tx, room_id).await {
                Ok(()) => {
                    subscribed += 1;
                    alive.push(tx);
                }
                Err(err) => {
                    tracing::warn!(
                        "NostrSignaler: dropping dead relay sender while subscribing: {:?}",
                        err
                    );
                }
            }
        }
        *senders = alive;

        if subscribed == 0 {
            return Err(mistlib_core::error::MistError::Signaling(
                "NostrSignaler: no relay connection is open".to_string(),
            ));
        }
        Ok(())
    }

    async fn current_room_id(&self) -> Option<String> {
        self.room_id.lock().await.clone()
    }

    async fn current_identity(&self) -> TemporarySignalingIdentity {
        self.rotated_identity
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| self.identity.clone())
    }

    fn session_epoch(&self) -> u64 {
        self.session_epoch.load(Ordering::SeqCst)
    }

    fn session_is_current(&self, expected_epoch: u64) -> bool {
        self.session_epoch() == expected_epoch
    }

    async fn clear_session_state(&self) {
        self.discovery_table.lock().await.clear();
        self.dedupe.lock().await.clear();
        self.message_dedupe.lock().await.clear();
        self.outgoing_sequences.lock().await.clear();
        self.incoming_sequences.lock().await.clear();
        self.requested_pubkeys.lock().await.clear();
        self.peer_sessions.lock().await.clear();
    }
}

#[async_trait]
impl Signaler for NostrSignaler {
    /// Refreshes this peer's `DiscoveryTable` binding on live traffic from
    /// *any* transport. Without it, a pair that finished its handshake and
    /// moved to the overlay stops producing relay messages, the binding
    /// lapses after `ttl_seconds`, and the next relay fallback (ICE restart,
    /// reconnect after a blip) is rejected in one direction only. See
    /// `Signaler::note_peer_alive`.
    async fn note_peer_alive(&self, peer: &NodeId) {
        self.discovery_table
            .lock()
            .await
            .touch_node(peer, self.codec_config.ttl_seconds);
    }

    async fn send_signaling(
        &self,
        to: &NodeId,
        msg: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        let MessageContent::Data(data) = msg else {
            return Err(mistlib_core::error::MistError::Signaling(
                "NostrSignaler: unsupported message type".to_string(),
            ));
        };

        // `Rejoin` is synthesized locally by this signaler purely to notify
        // its own transport (see `SignalingType::Rejoin`'s doc comment) and
        // must never be published to a relay. No caller is expected to reach
        // this with one -- the transport reacts to it without ever routing
        // it back through `Signaler::send_signaling` -- but guard here too
        // rather than rely on that.
        if data.signaling_type.is_local_only() {
            return Ok(());
        }

        self.set_room_id(&data.room_id).await?;

        if to.is_server() || to.is_broadcast() {
            if !data.room_id.is_empty() {
                self.publish_discovery(&data.room_id).await?;
            }
            return Ok(());
        }

        let pubkey = {
            let mut table = self.discovery_table.lock().await;
            table.pubkey_for_node(to)
        }
        .ok_or_else(|| mistlib_core::error::MistError::RouteNotFound(to.clone()))?;

        self.publish_message_to_pubkey(&pubkey, &data).await
    }

    async fn reset_session(&self) -> mistlib_core::error::Result<()> {
        let Some(room_id) = self.current_room_id().await else {
            return Ok(());
        };
        self.cancel_discovery_refresh();
        self.session_epoch.fetch_add(1, Ordering::SeqCst);
        *self.rotated_identity.lock().await = Some(TemporarySignalingIdentity::generate());
        self.clear_session_state().await;
        *self.local_joined_at.lock().await = Some(current_unix_millis());
        self.spawn_discovery_refresh(room_id.clone());
        self.publish_discovery(&room_id).await
    }

    async fn close(&self) -> mistlib_core::error::Result<()> {
        self.cancel_discovery_refresh();
        self.cancel_room_resubscribe();
        self.session_epoch.fetch_add(1, Ordering::SeqCst);
        if let Some(cancel) = self.reconnect_cancel.lock().await.take() {
            cancel.cancel();
        }
        self.senders.lock().await.clear();
        self.room_id.lock().await.take();
        self.clear_session_state().await;
        self.local_joined_at.lock().await.take();
        self.rotated_identity.lock().await.take();
        self.subscription_ids.lock().await.take();
        Ok(())
    }
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
