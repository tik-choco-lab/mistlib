use super::protocol::{build_query_message, WantRegistry, MSG_WANT};
use super::PeerResolver;
use crate::config::StorageTransferConfig;
use crate::error::{MistError, Result};
use crate::runtime::AsyncRuntime;
use crate::transport::Transport;
use crate::types::{DeliveryMethod, HostSendSync, NodeId};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{select_biased, FutureExt};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Semaphore;
use tracing::{debug, info};
use web_time::{Duration, Instant};

/// Discovery's stall-suspicion threshold, reset by accepted storage progress.
/// QUERY exchanges remain outstanding across bounded exponential retries. A
/// reachable outstanding WANT can additionally retain discovery until its cap:
/// HAVE_STATUS may wait behind bulk data during ordered transport recovery.
pub const DISCOVERY_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// Initial QUERY plus three retries: default silent windows are 5, 10, 20, 40 s
/// (75 s total, below ATTEMPT_CAP). Never replays a bulk WANT.
pub const MAX_DISCOVERY_QUERY_ROUNDS: u32 = 4;
/// Provider stall-suspicion threshold, reset by accepted per-sender progress.
/// A provider that has served during this attempt may recover until the fixed
/// cap unless a confirmed reset starts the bounded recovery/replay path; a provider that never responds fails at idle.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Allows four concurrent 1 MiB blocks over a roughly 30 KiB/s link, while
/// guaranteeing that even a provider trickling data cannot keep an attempt or
/// discovery alive forever. Each phase creates its own single, fixed cap.
pub const ATTEMPT_CAP: Duration = Duration::from_secs(300);
/// Bounds a call to four provider attempts and at most one discovery phase.
pub const MAX_ATTEMPTS: usize = 4;
/// At most two additional targeted WANTs after confirmed connection resets.
/// Silence on a live connection never triggers replay (avoids duplicate MiB transfers).
pub const MAX_RESET_REREQUESTS: usize = 2;
/// Reachability can return between idle checks; poll during reset or QUERY recovery.
const RECONNECT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Inspect at most this many input CIDs per prefetch call to bound QUERY fanout.
/// Later CIDs still use normal progress-aware discovery when resolved.
pub const MAX_PREFETCH_CIDS: usize = 256;

/// Supplies current session transports, snapshotted fresh on every call. A
/// provider may only be reachable through one of the active room sessions.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait TransportSource: HostSendSync {
    async fn transports(&self) -> Vec<Arc<dyn Transport>>;

    /// Stable, platform-neutral room identity paired with each transport.
    /// Embedders without rooms use the empty path. Snapshot before every send.
    async fn session_transports(&self) -> Vec<(String, Arc<dyn Transport>)> {
        self.transports()
            .await
            .into_iter()
            .map(|transport| (String::new(), transport))
            .collect()
    }
}

/// Fixed transports for tests and embedders without a session registry.
pub struct FixedTransportSource(pub Vec<Arc<dyn Transport>>);

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl TransportSource for FixedTransportSource {
    async fn transports(&self) -> Vec<Arc<dyn Transport>> {
        self.0.clone()
    }
}

/// Idle thresholds trigger liveness checks; each phase retains one fixed cap.
#[derive(Clone, Copy, Debug)]
pub struct ResolverTimings {
    /// Initial QUERY idle window; doubles per round, reset by registry progress.
    pub discovery_timeout: Duration,
    /// Recheck reachability after inactivity; also bounds reconnect recovery.
    pub idle_timeout: Duration,
    /// Fixed upper bound for each attempt or discovery, including sending and liveness checks.
    pub attempt_cap: Duration,
    pub max_attempts: usize,
}

impl Default for ResolverTimings {
    fn default() -> Self {
        Self {
            discovery_timeout: DISCOVERY_IDLE_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            attempt_cap: ATTEMPT_CAP,
            max_attempts: MAX_ATTEMPTS,
        }
    }
}

// Weak shared futures never keep a cancelled fetch alive. Completed/expired keys
// are pruned before insertion, with a hard bound even under unique-CID floods.
const MAX_IN_FLIGHT_FETCHES: usize = 1024;
type FetchResult = std::result::Result<Option<Vec<u8>>, String>;
#[cfg(not(target_arch = "wasm32"))]
type FetchFuture = futures_util::future::BoxFuture<'static, FetchResult>;
#[cfg(target_arch = "wasm32")]
type FetchFuture = futures_util::future::LocalBoxFuture<'static, FetchResult>;
type FetchKey = (Vec<String>, String);
type InFlight = HashMap<FetchKey, futures_util::future::WeakShared<FetchFuture>>;

#[derive(Clone)]
pub struct NetworkPeerResolver {
    transports: Arc<dyn TransportSource>,
    runtime: Arc<dyn AsyncRuntime>,
    registry: WantRegistry,
    timings: ResolverTimings,
    next_peer: Arc<AtomicUsize>,
    transfer: StorageTransferConfig,
    // Shared across every get/session using this resolver (not per room).
    provider_limits: Arc<Mutex<HashMap<NodeId, Weak<Semaphore>>>>,
    in_flight: Arc<Mutex<InFlight>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscoveryFailure {
    NoReachablePeers,
    QueryBudgetExhausted,
    TransportTimeout,
}

struct QueryPath {
    room: String,
    peer: NodeId,
    resets: tokio::sync::watch::Receiver<u64>,
}

impl DiscoveryFailure {
    fn message(self) -> &'static str {
        match self {
            Self::NoReachablePeers => "No reachable peers on the queried discovery paths",
            Self::QueryBudgetExhausted => "No provider answered within the discovery query budget",
            Self::TransportTimeout => "Block discovery transport timeout (fixed cap exceeded)",
        }
    }
}

// Also clean up if the caller drops an in-flight resolve future. This guard
// outlives the attempt/discovery receivers, so prune sees them as closed.
struct PruneOnDrop<'a>(&'a WantRegistry);

impl Drop for PruneOnDrop<'_> {
    fn drop(&mut self) {
        self.0.cancel_closed();
    }
}

impl NetworkPeerResolver {
    pub fn new(
        transports: Arc<dyn TransportSource>,
        runtime: Arc<dyn AsyncRuntime>,
        registry: WantRegistry,
        timings: ResolverTimings,
    ) -> Self {
        Self::new_with_transfer(
            transports,
            runtime,
            registry,
            timings,
            StorageTransferConfig::default(),
        )
    }

    pub fn new_with_transfer(
        transports: Arc<dyn TransportSource>,
        runtime: Arc<dyn AsyncRuntime>,
        registry: WantRegistry,
        timings: ResolverTimings,
        transfer: StorageTransferConfig,
    ) -> Self {
        Self {
            transports,
            runtime,
            registry,
            timings,
            next_peer: Arc::new(AtomicUsize::new(0)),
            transfer,
            provider_limits: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn provider_limit(&self, peer: &NodeId) -> Option<Arc<Semaphore>> {
        let limit = self.transfer.max_fetches_per_provider;
        if limit == 0 {
            return None;
        }
        let mut limits = self.provider_limits.lock().unwrap();
        if let Some(semaphore) = limits.get(peer).and_then(Weak::upgrade) {
            return Some(semaphore);
        }
        limits.retain(|_, semaphore| semaphore.strong_count() > 0);
        let semaphore = Arc::new(Semaphore::new((limit as usize).min(Semaphore::MAX_PERMITS)));
        limits.insert(peer.clone(), Arc::downgrade(&semaphore));
        Some(semaphore)
    }

    async fn resolve(&self, cid: &str) -> Result<Option<Vec<u8>>> {
        if !self.transfer.coalesce_fetches {
            return self
                .resolve_uncoalesced(cid)
                .await
                .map_err(MistError::Network);
        }
        // The public storage API is roomless and resolves over all joined
        // sessions. Scope sharing to their sorted room identities, so callers
        // in different room sets never inherit another room's in-flight work.
        let mut rooms: Vec<_> = self
            .transports
            .session_transports()
            .await
            .into_iter()
            .map(|(room, _)| room)
            .collect();
        rooms.sort();
        rooms.dedup();
        let key = (rooms, cid.to_owned());
        let shared = {
            let mut in_flight = self.in_flight.lock().unwrap();
            in_flight.retain(|_, future| future.upgrade().is_some_and(|f| f.peek().is_none()));
            if let Some(shared) = in_flight.get(&key).and_then(|future| future.upgrade()) {
                shared
            } else {
                if in_flight.len() >= MAX_IN_FLIGHT_FETCHES {
                    return Err(MistError::Network(
                        "Too many in-flight storage fetches".into(),
                    ));
                }
                let resolver = self.clone();
                let cid = cid.to_owned();
                let future = async move { resolver.resolve_uncoalesced(&cid).await };
                #[cfg(not(target_arch = "wasm32"))]
                let future: FetchFuture = future.boxed();
                #[cfg(target_arch = "wasm32")]
                let future: FetchFuture = future.boxed_local();
                let shared = future.shared();
                in_flight.insert(key, shared.downgrade().expect("new shared future"));
                shared
            }
        };
        // Every subscriber polls the same future: dropping the first transfers
        // driving to the remaining subscribers; dropping the last cancels it.
        shared.await.map_err(MistError::Network)
    }

    async fn any_reachable(&self, peers: &[NodeId]) -> bool {
        if peers.is_empty() {
            return false;
        }
        self.transports
            .transports()
            .await
            .iter()
            .any(|transport| peers.iter().any(|peer| transport.is_reachable(peer)))
    }

    async fn send_want(&self, attempt: u64, cid: &str, target: &NodeId) -> bool {
        let mut want = vec![MSG_WANT];
        want.extend_from_slice(cid.as_bytes());
        let want = Bytes::from(want);
        let mut sent = false;
        for (room, transport) in self.transports.session_transports().await {
            if !transport.is_reachable(target) {
                debug!(attempt, %cid, provider = %target.0, %room, "Storage: WANT path unreachable");
                continue;
            }
            // Attach before awaiting enqueue: even a synchronous reply or a
            // teardown while send is pending must see this attempt's path.
            let attached = self.registry.track_path(attempt, &room);
            let started = Instant::now();
            let result = transport
                .send(target, want.clone(), DeliveryMethod::ReliableOrdered)
                .await;
            debug!(attempt, %cid, provider = %target.0, %room, ?result, elapsed_ms = started.elapsed().as_millis(), "Storage: WANT enqueue result");
            if result.is_err() && attached {
                self.registry.untrack_path(attempt, &room);
            }
            if result.is_ok() {
                super::telemetry::STORAGE_COUNTERS
                    .want_sent
                    .fetch_add(1, Ordering::Relaxed);
            }
            sent |= result.is_ok();
        }
        sent
    }

    async fn attempt(&self, cid: &str, target: &NodeId) -> Option<Vec<u8>> {
        // Start the single cap before queuing for a permit. Idle/progress/reset
        // subscriptions start only after admission, so queue time is not idle.
        let cap = self.runtime.sleep(self.timings.attempt_cap).fuse();
        futures_util::pin_mut!(cap);
        let limit = self.provider_limit(target);
        let _permit = if let Some(limit) = limit {
            let acquire = limit.acquire_owned().fuse();
            futures_util::pin_mut!(acquire);
            select_biased! {
                _ = cap => return None,
                permit = acquire => Some(permit.ok()?),
            }
        } else {
            None
        };
        let registration = self.registry.register_attempt(cid, target.clone());
        let attempt_id = registration.id;
        let own_progress = registration.progress;
        let mut data = registration.data;
        let mut resets = registration.resets;
        let started = Instant::now();
        debug!(attempt = attempt_id, %cid, provider = %target.0, cap = ?self.timings.attempt_cap, "Storage: attempt started");
        match data.try_recv() {
            Ok(block) => return Some(block),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => return None,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
        }
        let mut progress = self.registry.progress(target);
        let progress_snapshot = progress.clone();
        let reset_snapshot = resets.clone();
        // One cap covers initial send, reset recovery, every replay and idle wait.
        // The data receiver stays alive across generations; sender_reset clears
        // assemblies because reconnects may negotiate different chunk layouts.
        let data = data.fuse();
        let attempt = async {
            let mut rerequests = 0;
            let mut was_serving = false;
            let sender_baseline = *progress.borrow();
            loop {
                let reset_during_send = {
                    let send = self.send_want(attempt_id, cid, target).fuse();
                    let reset = resets.changed().fuse();
                    futures_util::pin_mut!(send, reset);
                    select_biased! {
                        result = reset => {
                            if result.is_err() { return; }
                            true
                        },
                        sent = send => {
                            if !sent {
                                info!("PeerResolver: WANT for {} could not be sent to {}", cid, target.0);
                                return;
                            }
                            false
                        },
                    }
                };
                let mut logged_stall = false;
                if !reset_during_send {
                    loop {
                        let idle = async {
                            self.runtime.sleep(self.timings.idle_timeout).await;
                            self.any_reachable(std::slice::from_ref(target)).await
                        }.fuse();
                        let changed = progress.changed().fuse();
                        let reset = resets.changed().fuse();
                        futures_util::pin_mut!(idle, changed, reset);
                        select_biased! {
                            result = reset => {
                                if result.is_err() { return; }
                                break;
                            },
                            result = changed => {
                                if result.is_err() { return; }
                                was_serving = true;
                                if logged_stall {
                                    debug!(attempt = attempt_id, %cid, provider = %target.0, "Storage: attempt progress resumed");
                                    logged_stall = false;
                                }
                            },
                            reachable = idle => {
                                // Routing intentionally excludes Reconnecting peers (W9 send
                                // diversion). Route loss alone is unconfirmed suspicion, not
                                // absence. Only sender_reset may abandon the old stream and
                                // trigger replay; the original cap bounds this wait.
                                let own = *own_progress.borrow();
                                info!(attempt = attempt_id, %cid, provider = %target.0, reachable,
                                    reset_generation = *reset_snapshot.borrow(),
                                    own_received = own.received, own_accepted = own.accepted,
                                    sender_accepted = (*progress_snapshot.borrow()).wrapping_sub(sender_baseline),
                                    own_age_ms = ?own.last_accepted.map(|at| at.elapsed().as_millis()),
                                    sender_age_ms = ?self.registry.sender_last_accepted(target).map(|at| at.elapsed().as_millis()),
                                    paths = ?self.registry.attempt_paths(attempt_id),
                                    elapsed_ms = started.elapsed().as_millis(), was_serving,
                                    "Storage: attempt stall window");
                                if was_serving {
                                    logged_stall = true;
                                    continue;
                                }
                                info!("PeerResolver: no data from {} for {:?} while fetching {} (reachable={})", target.0, self.timings.idle_timeout, cid, reachable);
                                return;
                            },
                        }
                    }
                }
                info!(attempt = attempt_id, generation = *resets.borrow(), "PeerResolver: sender {} reset while fetching {}", target.0, cid);
                if rerequests >= MAX_RESET_REREQUESTS {
                    info!("PeerResolver: re-request bound reached for {} from {}", cid, target.0);
                    return;
                }
                // Repeated resets while disconnected coalesce into one recovery
                // window. Neither progress nor resets can extend this idle budget.
                let recovery = async {
                    loop {
                        // Coalesce known resets before checking the route. A reset
                        // racing that check stays unseen and interrupts the send.
                        resets.borrow_and_update();
                        if self.any_reachable(std::slice::from_ref(target)).await {
                            return;
                        }
                        self.runtime.sleep(RECONNECT_POLL_INTERVAL).await;
                    }
                }.fuse();
                let idle = self.runtime.sleep(self.timings.idle_timeout).fuse();
                futures_util::pin_mut!(recovery, idle);
                select_biased! {
                    _ = idle => {
                        info!("PeerResolver: {} did not become reachable within {:?} after reset during {}", target.0, self.timings.idle_timeout, cid);
                        return;
                    },
                    _ = recovery => {},
                }
                // Keep W12's accepted service evidence for this attempt.
                rerequests += 1;
                info!("PeerResolver: re-requesting {} from {} after reset ({}/{})", cid, target.0, rerequests, MAX_RESET_REREQUESTS);
            }
        }.fuse();
        futures_util::pin_mut!(cap, data, attempt);
        let result = select_biased! {
            _ = cap => {
                info!(attempt = attempt_id, "PeerResolver: {} from {} exceeded the attempt cap", cid, target.0);
                None
            },
            result = data => result.ok(),
            // A synchronous send can fulfill data and close its watch senders
            // in the same poll. Read the completed block before reporting failure.
            _ = attempt => data.now_or_never().and_then(|result| result.ok()),
        };
        let own = *own_progress.borrow();
        debug!(attempt = attempt_id, %cid, provider = %target.0, completed = result.is_some(),
            own_received = own.received, own_accepted = own.accepted,
            bytes = result.as_ref().map(Vec::len), elapsed_ms = started.elapsed().as_millis(),
            "Storage: attempt ended");
        result
    }

    /// Retry the same small query on the original room paths. A peer reachable
    /// only in another room cannot keep a dead query path alive. Snapshot before
    /// enqueue so teardown/reconnect during the await cannot change recipients.
    async fn broadcast_query(
        &self,
        cid: &str,
        queried: &mut Vec<QueryPath>,
        initial: bool,
    ) -> bool {
        let query = Bytes::from(build_query_message(cid));
        let mut enqueued = false;
        for (room, transport) in self.transports.session_transports().await {
            if initial {
                queried.extend(
                    transport
                        .get_connected_nodes()
                        .into_iter()
                        .map(|peer| QueryPath {
                            resets: self.registry.query_path_resets(&room, &peer),
                            room: room.clone(),
                            peer,
                        }),
                );
            } else {
                let mut reachable = false;
                for path in queried.iter_mut().filter(|path| path.room == room) {
                    if transport.is_reachable(&path.peer) {
                        // A recovered path gets a new QUERY. Observe the current
                        // generation before enqueue so resets during it survive.
                        path.resets.borrow_and_update();
                        reachable = true;
                    }
                }
                if !reachable {
                    continue;
                }
            }
            let result = transport
                .broadcast(query.clone(), DeliveryMethod::ReliableOrdered)
                .await;
            if result.is_ok() {
                super::telemetry::STORAGE_COUNTERS
                    .query_sent
                    .fetch_add(1, Ordering::Relaxed);
            }
            enqueued = true;
            debug!(%cid, %room, ?result, initial, "Storage: discovery QUERY enqueue result");
        }
        enqueued
    }

    async fn query_path_state(&self, queried: &[QueryPath]) -> (bool, bool) {
        let transports = self.transports.session_transports().await;
        let mut reachable = false;
        let mut temporary = false;
        for path in queried {
            // Removal of the room itself is teardown. Reachability loss within
            // an existing room needs an explicit sender_reset/room_reset.
            if let Some((_, transport)) = transports.iter().find(|(room, _)| room == &path.room) {
                if transport.is_reachable(&path.peer) {
                    reachable = true;
                } else if !path.resets.has_changed().unwrap_or(true) {
                    temporary = true;
                }
            }
        }
        (reachable, temporary)
    }

    async fn discover(
        &self,
        cid: &str,
        tried: &HashSet<NodeId>,
    ) -> std::result::Result<Vec<NodeId>, DiscoveryFailure> {
        let _prune = PruneOnDrop(&self.registry);
        let mut notified = self.registry.register_peer_notifier(cid);
        let mut progress = self.registry.any_progress();
        // All sends, retries and progress share one cap; cancellation drops all
        // waits directly, with no background retry task or pending WANT added.
        let cap = self.runtime.sleep(self.timings.attempt_cap).fuse();
        let exchange = async {
            let mut queried = Vec::new();
            let mut rounds = 1;
            self.broadcast_query(cid, &mut queried, true).await;
            loop {
                let delay = self
                    .timings
                    .discovery_timeout
                    .saturating_mul(1 << (rounds - 1));
                let idle = self.runtime.sleep(delay).fuse();
                let changed = progress.changed().fuse();
                futures_util::pin_mut!(idle, changed);
                select_biased! {
                    result = changed => {
                        if result.is_err() { return DiscoveryFailure::TransportTimeout; }
                        continue;
                    },
                    _ = idle => {},
                }
                loop {
                    let (reachable, temporary) = self.query_path_state(&queried).await;
                    let outstanding = self
                        .any_reachable(&self.registry.outstanding_senders())
                        .await;
                    if reachable && rounds < MAX_DISCOVERY_QUERY_ROUNDS {
                        if self.broadcast_query(cid, &mut queried, false).await {
                            rounds += 1;
                            debug!(%cid, round = rounds, "Storage: retried discovery QUERY after silence");
                            break;
                        }
                        // Reachability changed between snapshot and enqueue.
                        self.runtime.sleep(RECONNECT_POLL_INTERVAL).await;
                        continue;
                    }
                    if !outstanding {
                        if !reachable && !temporary {
                            return DiscoveryFailure::NoReachablePeers;
                        }
                        if rounds == MAX_DISCOVERY_QUERY_ROUNDS {
                            return DiscoveryFailure::QueryBudgetExhausted;
                        }
                    }
                    if !reachable && temporary {
                        // Suspicion removes overlay routes while retaining the
                        // connection. Neither abandon QUERY nor spend rounds;
                        // resume retries as soon as this room path recovers.
                        self.runtime.sleep(RECONNECT_POLL_INTERVAL).await;
                        continue;
                    }
                    break;
                }
                // Reachable outstanding WANTs retain the existing waiting rule,
                // even after the QUERY round budget or query routes are exhausted.
            }
        }
        .fuse();
        let discover = async {
            loop {
                let peers: Vec<_> = self
                    .registry
                    .get_peers(cid)
                    .into_iter()
                    .filter(|peer| !tried.contains(peer))
                    .collect();
                if !peers.is_empty() {
                    return peers;
                }
                let _ = (&mut notified).await;
                // Rearm before reading the cache so updates cannot be missed.
                notified = self.registry.register_peer_notifier(cid);
            }
        }
        .fuse();
        futures_util::pin_mut!(cap, exchange, discover);
        let reason = select_biased! {
            _ = cap => DiscoveryFailure::TransportTimeout,
            peers = discover => return Ok(peers),
            reason = exchange => reason,
        };
        info!(%cid, reason = reason.message(), "Storage: discovery ended without a provider");
        Err(reason)
    }

    async fn resolve_uncoalesced(&self, cid: &str) -> FetchResult {
        let _prune = PruneOnDrop(&self.registry);
        let mut peers = self.registry.get_peers(cid);
        if !peers.is_empty() {
            let start = self.next_peer.fetch_add(1, Ordering::Relaxed) % peers.len();
            peers.rotate_left(start);
        }
        let mut tried = HashSet::new();
        let mut discovered = false;
        while tried.len() < self.timings.max_attempts {
            while !peers.is_empty() {
                // Keep round-robin order among free providers; do not queue
                // behind a saturated one while a known alternative is free.
                let index = if self.transfer.max_fetches_per_provider == 0 {
                    0
                } else {
                    peers
                        .iter()
                        .position(|peer| {
                            !tried.contains(peer)
                                && self
                                    .provider_limit(peer)
                                    .is_some_and(|limit| limit.available_permits() > 0)
                        })
                        .unwrap_or(0)
                };
                let peer = peers.remove(index);
                if tried.len() >= self.timings.max_attempts {
                    return Ok(None);
                }
                if !tried.insert(peer.clone()) {
                    continue;
                }
                debug!("PeerResolver: targeted WANT for {} to {}", cid, peer.0);
                let result = self.attempt(cid, &peer).await;
                self.registry.cancel_closed();
                if result.is_some() {
                    return Ok(result);
                }
            }
            if tried.len() >= self.timings.max_attempts {
                return Ok(None);
            }
            if discovered {
                // More QUERY replies may have arrived while the last provider
                // was being tried. Include those without a second broadcast.
                peers = self
                    .registry
                    .get_peers(cid)
                    .into_iter()
                    .filter(|peer| !tried.contains(peer))
                    .collect();
                if peers.is_empty() {
                    return Ok(None);
                }
                continue;
            }
            peers = self
                .discover(cid, &tried)
                .await
                .map_err(|reason| format!("{}: {}", reason.message(), cid))?;
            debug!(
                "PeerResolver: discovery for {} found {} new provider(s)",
                cid,
                peers.len()
            );
            discovered = true;
        }
        Ok(None)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl PeerResolver for NetworkPeerResolver {
    async fn resolve_block_result(&self, cid: &str) -> Result<Option<Vec<u8>>> {
        self.resolve(cid).await
    }

    async fn prefetch_providers(&self, cids: &[String]) {
        let transports = self.transports.transports().await;
        let mut seen = HashSet::new();
        for cid in cids.iter().take(MAX_PREFETCH_CIDS) {
            if !seen.insert(cid) || !self.registry.get_peers(cid).is_empty() {
                continue;
            }
            let query = Bytes::from(build_query_message(cid));
            for transport in &transports {
                if transport
                    .broadcast(query.clone(), DeliveryMethod::ReliableOrdered)
                    .await
                    .is_ok()
                {
                    super::telemetry::STORAGE_COUNTERS
                        .query_sent
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    async fn resolve_block(&self, cid: &str) -> Option<Vec<u8>> {
        self.resolve(cid).await.ok().flatten()
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::error::{MistError, Result};
    use crate::runtime::BoxTask;
    use crate::storage::protocol::{
        build_have_chunk_message, parse_have_chunk_message, parse_query_message,
        parse_want_message, HAVE_CHUNK_SIZE,
    };
    use crate::transport::NetworkEventHandler;
    use crate::types::ConnectionState;
    use futures_util::stream::{self, StreamExt};
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::sync::Mutex;
    use tokio::time::Instant;

    // CIDv1/raw fixtures computed from the former labels with compute_cid.
    const TEST_CID: &str = "bafkreicuerrgxezkf745depqtasepklz7xoyws7ckhusinymwzuqbxybry";
    const TEST_CID_1: &str = "bafkreifh4syzuw3xegwuej4254grej2oars6la4qothbz6qwflcwyojkgm";
    const OTHER_CID: &str = "bafkreigzfgfbbunqonmdpxcl3bo2yza3b46o6j5epzovhjkpf47vwl6p7i";
    const LATER_CID: &str = "bafkreia5skb5qshksqnm4h7a2i3y56fxablkbvgrmsfzlizc3eawhz4cqu";
    const MISSING_CID: &str = "bafkreih7uy2yhx5gobvypuuexbvq22j2cypeqqfk2lc46225e7b3syq7pu";
    const TEST_CIDS: [&str; 6] = [
        "bafkreigks6arfsq3xxfpvqrrwonchxcnu6do76auprhhfomao6c273sixm",
        "bafkreib6epubmabzlffdhckpmvsodmjuro6xuaei2qwevs3t52xnlhaatu",
        "bafkreibopuwahkkqplrgl3hvwu2wrbnfgoj2eau5eqjzjglsmwq2ewxpyy",
        "bafkreiayvq7hgq7qc2eqyuiosp4tkjqrnhm6h5lfinsctaypv4etj5hy4q",
        "bafkreib7pg5xwq23auzbmuo257jxjtogqhoan6vgly3u4obtpoemubdn5i",
        "bafkreibff4imqnqq5pfbubm4boxievplul4vxzgr266pvcoxesfifwprce",
    ];

    struct TestRuntime;

    #[async_trait]
    impl AsyncRuntime for TestRuntime {
        fn spawn(&self, future: BoxTask) {
            tokio::spawn(future);
        }

        async fn sleep(&self, duration: Duration) {
            tokio::time::sleep(duration).await;
        }
    }

    #[derive(Clone)]
    enum Response {
        Silent,
        Reject,
        Immediate(Vec<u8>),
        AfterReplay(Vec<u8>),
        Chunks {
            block: Vec<u8>,
            delay: Duration,
            limit: usize,
        },
        HangingSend,
        HangingReplay,
        HangingUntilReplay(Vec<u8>),
    }

    struct FakeTransport {
        room: String,
        registry: WantRegistry,
        responses: HashMap<NodeId, Response>,
        responses_by_cid: HashMap<String, Response>,
        sends: Mutex<Vec<(NodeId, String, Instant)>>,
        broadcasts: Mutex<Vec<Vec<u8>>>,
        advertisements: Vec<(NodeId, Duration)>,
        query_peers: Vec<NodeId>,
        advertise_after_round: usize,
        hang_after_query: bool,
        reset_during_query: bool,
        ordered_advertisements: bool,
        connection_states: Mutex<HashMap<NodeId, ConnectionState>>,
        // A responder sends whole blocks serially, as real storage handlers do.
        queues: Mutex<HashMap<NodeId, Arc<tokio::sync::Mutex<()>>>>,
    }

    impl FakeTransport {
        fn new(registry: &WantRegistry, responses: Vec<(&str, Response)>) -> Self {
            Self {
                room: String::new(),
                registry: registry.clone(),
                responses: responses.into_iter().map(|(id, r)| (peer(id), r)).collect(),
                responses_by_cid: HashMap::new(),
                sends: Mutex::new(Vec::new()),
                broadcasts: Mutex::new(Vec::new()),
                advertisements: Vec::new(),
                query_peers: Vec::new(),
                advertise_after_round: 1,
                hang_after_query: false,
                reset_during_query: false,
                ordered_advertisements: false,
                connection_states: Mutex::new(HashMap::new()),
                queues: Mutex::new(HashMap::new()),
            }
        }

        fn targets(&self) -> Vec<NodeId> {
            self.sends
                .lock()
                .unwrap()
                .iter()
                .map(|(id, _, _)| id.clone())
                .collect()
        }

        fn set_connection_state(&self, node: &NodeId, state: ConnectionState) {
            self.connection_states
                .lock()
                .unwrap()
                .insert(node.clone(), state);
        }

        fn assert_queries_only(&self, count: usize) {
            let broadcasts = self.broadcasts.lock().unwrap();
            assert_eq!(broadcasts.len(), count);
            assert!(broadcasts
                .iter()
                .all(|data| parse_query_message(data).is_some()));
        }

        fn queue(&self, node: &NodeId) -> Arc<tokio::sync::Mutex<()>> {
            self.queues
                .lock()
                .unwrap()
                .entry(node.clone())
                .or_default()
                .clone()
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn start(&self, _: Arc<dyn NetworkEventHandler>) -> Result<()> {
            Ok(())
        }

        async fn send(&self, node: &NodeId, data: Bytes, _: DeliveryMethod) -> Result<()> {
            let cid = parse_want_message(&data).expect("only targeted WANTs should be sent");
            self.sends
                .lock()
                .unwrap()
                .push((node.clone(), cid.clone(), Instant::now()));
            match self
                .responses_by_cid
                .get(&cid)
                .or_else(|| self.responses.get(node))
                .cloned()
                .unwrap_or(Response::Reject)
            {
                Response::Reject => return Err(MistError::NodeNotFound(node.clone())),
                Response::Silent => {}
                Response::Immediate(data) => self.registry.fulfill_in(&self.room, &cid, node, data),
                Response::AfterReplay(data) => {
                    let count = self
                        .sends
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(target, sent_cid, _)| target == node && sent_cid == &cid)
                        .count();
                    if count > 1 {
                        self.registry
                            .fulfill_chunk_in(&self.room, &cid, node, 0, 1, data);
                    } else {
                        // The replacement negotiates a different chunk layout.
                        self.registry.fulfill_chunk_in(
                            &self.room,
                            &cid,
                            node,
                            0,
                            2,
                            b"old-partial".to_vec(),
                        );
                    }
                }
                Response::HangingSend => futures_util::future::pending().await,
                Response::HangingUntilReplay(data) => {
                    let count = self.sends.lock().unwrap().len();
                    if count > 1 {
                        self.registry.fulfill_in(&self.room, &cid, node, data);
                    } else {
                        futures_util::future::pending().await
                    }
                }
                Response::HangingReplay => {
                    if self.sends.lock().unwrap().len() > 1 {
                        futures_util::future::pending().await
                    }
                }
                Response::Chunks {
                    block,
                    delay,
                    limit,
                } => {
                    let queue = self.queue(node);
                    let registry = self.registry.clone();
                    let node = node.clone();
                    let room = self.room.clone();
                    tokio::spawn(async move {
                        let _guard = queue.lock().await;
                        let total = block.len().div_ceil(HAVE_CHUNK_SIZE) as u16;
                        for (index, data) in block.chunks(HAVE_CHUNK_SIZE).take(limit).enumerate() {
                            tokio::time::sleep(delay).await;
                            registry.fulfill_chunk_in(
                                &room,
                                &cid,
                                &node,
                                index as u16,
                                total,
                                data.to_vec(),
                            );
                        }
                    });
                }
            }
            Ok(())
        }

        async fn broadcast(&self, data: Bytes, _: DeliveryMethod) -> Result<()> {
            self.broadcasts.lock().unwrap().push(data.to_vec());
            if self.reset_during_query {
                for node in &self.query_peers {
                    self.set_connection_state(node, ConnectionState::Disconnected);
                }
                self.registry.room_reset(&self.room);
            }
            let cid = parse_query_message(&data).expect("WANT must never be broadcast");
            let round = self.broadcasts.lock().unwrap().len();
            for (provider, delay) in self
                .advertisements
                .clone()
                .into_iter()
                .filter(|_| round >= self.advertise_after_round)
            {
                if delay.is_zero() && !self.ordered_advertisements {
                    self.registry.register_peer(&cid, provider);
                } else {
                    let queue = self.ordered_advertisements.then(|| self.queue(&provider));
                    let registry = self.registry.clone();
                    let cid = cid.clone();
                    tokio::spawn(async move {
                        // Model HAVE_STATUS behind all previously queued block
                        // chunks on this provider's reliable ordered channel.
                        let _guard = match queue {
                            Some(queue) => Some(queue.lock_owned().await),
                            None => None,
                        };
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        registry.register_peer(&cid, provider);
                    });
                }
            }
            if self.hang_after_query {
                futures_util::future::pending::<()>().await;
            }
            Ok(())
        }

        fn get_connection_state(&self, node: &NodeId) -> ConnectionState {
            self.connection_states
                .lock()
                .unwrap()
                .get(node)
                .copied()
                .unwrap_or(ConnectionState::Connected)
        }
        async fn connect(&self, _: &NodeId) -> Result<()> {
            Ok(())
        }
        async fn disconnect(&self, _: &NodeId) -> Result<()> {
            Ok(())
        }
        fn get_connected_nodes(&self) -> Vec<NodeId> {
            self.query_peers
                .iter()
                .filter(|peer| self.is_reachable(peer))
                .cloned()
                .collect()
        }
    }

    fn peer(id: &str) -> NodeId {
        NodeId(id.into())
    }

    fn resolver(registry: &WantRegistry, transport: Arc<FakeTransport>) -> NetworkPeerResolver {
        NetworkPeerResolver::new(
            Arc::new(FixedTransportSource(vec![transport])),
            Arc::new(TestRuntime),
            registry.clone(),
            ResolverTimings::default(),
        )
    }

    struct RoomTransports(Vec<(String, Arc<dyn Transport>)>);

    #[async_trait]
    impl TransportSource for RoomTransports {
        async fn transports(&self) -> Vec<Arc<dyn Transport>> {
            self.0
                .iter()
                .map(|(_, transport)| transport.clone())
                .collect()
        }
        async fn session_transports(&self) -> Vec<(String, Arc<dyn Transport>)> {
            self.0.clone()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn secondary_room_teardown_does_not_replay_or_destroy_main_room_attempt() {
        // Neither an unreachable path nor a failed enqueue used that room.
        for secondary_state in [ConnectionState::Disconnected, ConnectionState::Connected] {
            let registry = WantRegistry::new();
            let main = Arc::new(FakeTransport::new(
                &registry,
                vec![("remote", Response::Silent)],
            ));
            let secondary = Arc::new(FakeTransport::new(
                &registry,
                vec![("remote", Response::Reject)],
            ));
            secondary.set_connection_state(&peer("remote"), secondary_state);
            let resolver = NetworkPeerResolver::new(
                Arc::new(RoomTransports(vec![
                    ("main".into(), main.clone()),
                    ("secondary".into(), secondary.clone()),
                ])),
                Arc::new(TestRuntime),
                registry.clone(),
                ResolverTimings::default(),
            );
            let incoming = registry.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                incoming.fulfill_chunk_in("main", TEST_CID, &peer("remote"), 0, 2, vec![7]);
                incoming.sender_reset(&peer("remote"), "secondary");
                tokio::time::sleep(Duration::from_secs(1)).await;
                incoming.fulfill_chunk_in("main", TEST_CID, &peer("remote"), 1, 2, vec![8]);
            });
            assert_eq!(
                resolver.attempt(TEST_CID, &peer("remote")).await,
                Some(vec![7, 8])
            );
            assert_eq!(main.targets(), [peer("remote")]);
            assert_eq!(
                secondary.targets().len(),
                usize::from(secondary_state == ConnectionState::Connected)
            );
        }
    }

    struct DiscardActions;

    impl crate::overlay::ActionHandler for DiscardActions {
        fn handle_action(&self, _: crate::action::OverlayAction) {}
    }

    fn overlay_transport() -> Arc<crate::overlay::transport::OverlayTransport> {
        Arc::new(crate::overlay::transport::OverlayTransport {
            router: Arc::new(crate::overlay::OverlayRouter::new(
                &crate::config::Config::default(),
                Arc::new(Mutex::new(crate::overlay::node_store::NodeStore::new())),
                peer("local"),
            )),
            action_handler: Arc::new(DiscardActions),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn overlay_reachability_uses_routes_despite_unconditional_connection_state() {
        let registry = WantRegistry::new();
        let overlay = overlay_transport();
        let resolver = NetworkPeerResolver::new(
            Arc::new(FixedTransportSource(vec![overlay.clone()])),
            Arc::new(TestRuntime),
            registry.clone(),
            ResolverTimings::default(),
        );
        let remote = peer("routed");
        let relay = peer("relay");
        assert_eq!(
            overlay.get_connection_state(&remote),
            ConnectionState::Connected
        );
        assert!(!resolver.any_reachable(std::slice::from_ref(&remote)).await);
        {
            let mut rt = overlay.router.routing_table.lock().unwrap();
            rt.on_connected(relay.clone());
            rt.add_route(remote.clone(), relay.clone());
        }
        assert!(resolver.any_reachable(std::slice::from_ref(&remote)).await);
        assert!(resolver.any_reachable(std::slice::from_ref(&relay)).await);
        overlay
            .router
            .routing_table
            .lock()
            .unwrap()
            .on_disconnected(&relay);
        assert!(!resolver.any_reachable(std::slice::from_ref(&remote)).await);
        // An outstanding WANT must not make discovery wait to cap when the
        // overlay's legacy getter claims Connected but there is no route.
        let _want = registry.register_from(OTHER_CID, remote);
        let start = Instant::now();
        assert!(resolver.discover(TEST_CID, &HashSet::new()).await.is_err());
        assert_eq!(start.elapsed(), DISCOVERY_IDLE_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn serving_overlay_provider_survives_route_removal_during_suspect_grace() {
        let registry = WantRegistry::new();
        let overlay = overlay_transport();
        let remote = peer("remote");
        overlay
            .router
            .sync_connection_states(&[(remote.clone(), ConnectionState::Connected)]);
        let resolver = NetworkPeerResolver::new(
            Arc::new(FixedTransportSource(vec![overlay.clone()])),
            Arc::new(TestRuntime),
            registry.clone(),
            ResolverTimings::default(),
        );
        let incoming = registry.clone();
        let provider = remote.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            incoming.fulfill_chunk_from(TEST_CID, &provider, 0, 2, vec![7]);
            // The normal tick excludes Reconnecting from routing without a
            // disconnect event or sender_reset. Preserve that send policy.
            overlay
                .router
                .sync_connection_states(&[(provider.clone(), ConnectionState::Reconnecting)]);
            assert!(!overlay.is_reachable(&provider));
            tokio::time::sleep(2 * IDLE_TIMEOUT + Duration::from_secs(1)).await;
            overlay
                .router
                .sync_connection_states(&[(provider.clone(), ConnectionState::Connected)]);
            incoming.fulfill_chunk_from(TEST_CID, &provider, 1, 2, vec![8]);
        });
        let start = Instant::now();
        assert_eq!(resolver.attempt(TEST_CID, &remote).await, Some(vec![7, 8]));
        assert_eq!(start.elapsed(), 2 * IDLE_TIMEOUT + Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn reset_and_fast_reconnect_replays_want_and_completes_new_chunk_layout() {
        let registry = WantRegistry::new();
        let payload = b"verified replacement block".to_vec();
        let cid = crate::storage::cid::compute_cid(&payload, crate::storage::cid::MULTICODEC_RAW);
        registry.register_peer(&cid, peer("remote"));
        let mut transport = FakeTransport::new(
            &registry,
            vec![("remote", Response::AfterReplay(payload.clone()))],
        );
        transport.room = "main".into();
        let transport = Arc::new(transport);
        let resetting = transport.clone();
        let events = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            resetting.set_connection_state(&peer("remote"), ConnectionState::Disconnected);
            events.sender_reset(&peer("remote"), "main");
            tokio::time::sleep(Duration::from_secs(5)).await;
            resetting.set_connection_state(&peer("remote"), ConnectionState::Connected);
        });
        let start = Instant::now();
        let resolver = NetworkPeerResolver::new(
            Arc::new(RoomTransports(vec![("main".into(), transport.clone())])),
            Arc::new(TestRuntime),
            registry.clone(),
            ResolverTimings::default(),
        );
        let block = resolver.resolve_block(&cid).await.unwrap();
        assert_eq!(block, payload);
        assert!(crate::storage::cid::verify_cid(
            &cid,
            &block,
            crate::storage::cid::MULTICODEC_RAW
        ));
        assert!(start.elapsed() >= Duration::from_secs(6));
        assert!(start.elapsed() <= Duration::from_secs(6) + RECONNECT_POLL_INTERVAL);
        assert_eq!(transport.targets(), [peer("remote"), peer("remote")]);
        assert!(registry.outstanding_senders().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn reset_without_reconnect_fails_over_after_idle_from_reset() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("remote"));
        registry.register_peer(TEST_CID, peer("good"));
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![
                ("remote", Response::AfterReplay(vec![1])),
                ("good", Response::Immediate(vec![2])),
            ],
        ));
        let resetting = transport.clone();
        let events = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            resetting.set_connection_state(&peer("remote"), ConnectionState::Disconnected);
            events.sender_reset(&peer("remote"), "");
        });
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(vec![2])
        );
        assert_eq!(start.elapsed(), Duration::from_secs(1) + IDLE_TIMEOUT);
        assert_eq!(transport.targets(), [peer("remote"), peer("good")]);
    }

    #[tokio::test(start_paused = true)]
    async fn reconnects_between_polls_respect_rerequest_bound() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("remote"));
        registry.register_peer(TEST_CID, peer("good"));
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![
                ("remote", Response::Silent),
                ("good", Response::Immediate(vec![2])),
            ],
        ));
        let events = registry.clone();
        tokio::spawn(async move {
            for _ in 0..=MAX_RESET_REREQUESTS {
                tokio::time::sleep(Duration::from_secs(1)).await;
                // Already reachable again when polled: the retained event is essential.
                events.sender_reset(&peer("remote"), "");
            }
        });
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(vec![2])
        );
        assert_eq!(
            start.elapsed(),
            Duration::from_secs((MAX_RESET_REREQUESTS + 1) as u64)
        );
        assert_eq!(
            transport
                .targets()
                .iter()
                .filter(|p| **p == peer("remote"))
                .count(),
            1 + MAX_RESET_REREQUESTS
        );
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_resets_do_not_extend_recovery_idle_or_original_attempt_cap() {
        for cap in [Duration::from_secs(12), Duration::from_secs(30)] {
            let registry = WantRegistry::new();
            let transport = Arc::new(FakeTransport::new(
                &registry,
                vec![("remote", Response::Silent)],
            ));
            let resetting = transport.clone();
            let events = registry.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(8)).await;
                resetting.set_connection_state(&peer("remote"), ConnectionState::Disconnected);
                events.sender_reset(&peer("remote"), "");
                for _ in 0..15 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    events.sender_reset(&peer("remote"), "");
                }
            });
            let mut resolver = resolver(&registry, transport.clone());
            resolver.timings.attempt_cap = cap;
            let start = Instant::now();
            assert!(resolver.attempt(TEST_CID, &peer("remote")).await.is_none());
            assert_eq!(
                start.elapsed(),
                cap.min(Duration::from_secs(8) + IDLE_TIMEOUT)
            );
            assert_eq!(transport.targets(), [peer("remote")]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn replay_send_and_recovery_share_the_original_cap() {
        let registry = WantRegistry::new();
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![("remote", Response::HangingReplay)],
        ));
        let resetting = transport.clone();
        let events = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(8)).await;
            resetting.set_connection_state(&peer("remote"), ConnectionState::Disconnected);
            events.sender_reset(&peer("remote"), "");
            tokio::time::sleep(Duration::from_secs(3)).await;
            resetting.set_connection_state(&peer("remote"), ConnectionState::Connected);
        });
        let mut resolver = resolver(&registry, transport.clone());
        resolver.timings.attempt_cap = Duration::from_secs(12);
        let start = Instant::now();
        assert!(resolver.attempt(TEST_CID, &peer("remote")).await.is_none());
        assert_eq!(start.elapsed(), resolver.timings.attempt_cap);
        assert_eq!(transport.targets(), [peer("remote"), peer("remote")]);
    }

    #[tokio::test(start_paused = true)]
    async fn reset_interrupts_a_hanging_initial_send_and_replays() {
        let registry = WantRegistry::new();
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![("remote", Response::HangingUntilReplay(vec![8]))],
        ));
        let events = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            events.sender_reset(&peer("remote"), "");
        });
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .attempt(TEST_CID, &peer("remote"))
                .await,
            Some(vec![8])
        );
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        assert_eq!(transport.targets(), [peer("remote"), peer("remote")]);
    }

    #[tokio::test(start_paused = true)]
    async fn serving_evidence_survives_reset_without_silence_triggered_replays() {
        let registry = WantRegistry::new();
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![("remote", Response::Silent)],
        ));
        let events = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            events.fulfill_chunk_from(TEST_CID, &peer("remote"), 0, 2, vec![1]);
            tokio::time::sleep(Duration::from_secs(1)).await;
            events.sender_reset(&peer("remote"), "");
        });
        let mut resolver = resolver(&registry, transport.clone());
        resolver.timings.attempt_cap = Duration::from_secs(30);
        let start = Instant::now();
        assert!(resolver.attempt(TEST_CID, &peer("remote")).await.is_none());
        assert_eq!(start.elapsed(), resolver.timings.attempt_cap);
        // Exactly the initial WANT and the reset-triggered replay, despite
        // crossing multiple idle windows on the live replacement connection.
        assert_eq!(transport.targets(), [peer("remote"), peer("remote")]);
    }

    fn slow_block() -> Response {
        Response::Chunks {
            block: vec![7; 1024 * 1024],
            delay: Duration::from_millis(320), // 16 KiB per 320 ms = 50 KiB/s.
            limit: usize::MAX,
        }
    }

    #[test]
    fn have_chunk_message_roundtrips() {
        let msg = build_have_chunk_message(TEST_CID_1, 1, 3, b"payload");
        let parsed = parse_have_chunk_message(&msg).unwrap();
        assert_eq!(parsed, (TEST_CID_1.into(), 1, 3, b"payload".to_vec()));
    }

    #[tokio::test]
    async fn registry_reassembles_have_chunks() {
        let registry = WantRegistry::new();
        let rx = registry.register(TEST_CID_1);
        registry.fulfill_chunk(TEST_CID_1, 1, 3, b"bb".to_vec());
        registry.fulfill_chunk(TEST_CID_1, 0, 3, b"aa".to_vec());
        registry.fulfill_chunk(TEST_CID_1, 2, 3, b"cc".to_vec());
        assert_eq!(rx.await.unwrap(), b"aabbcc");
    }

    #[tokio::test(start_paused = true)]
    async fn slow_mebibyte_succeeds_with_continuous_progress() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("slow"));
        let transport = Arc::new(FakeTransport::new(&registry, vec![("slow", slow_block())]));
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(vec![7; 1024 * 1024])
        );
        assert_eq!(start.elapsed(), Duration::from_millis(20_480));
        transport.assert_queries_only(0);
    }

    #[tokio::test(start_paused = true)]
    async fn four_serial_blocks_share_sender_progress() {
        let registry = WantRegistry::new();
        let cids = [TEST_CIDS[0], TEST_CIDS[1], TEST_CIDS[2], TEST_CIDS[3]];
        for cid in cids {
            registry.register_peer(cid, peer("slow"));
        }
        let transport = Arc::new(FakeTransport::new(&registry, vec![("slow", slow_block())]));
        let resolver = resolver(&registry, transport.clone());
        let start = Instant::now();
        let results =
            futures_util::future::join_all(cids.iter().map(|cid| resolver.resolve_block(cid)))
                .await;
        assert!(results
            .into_iter()
            .all(|block| block == Some(vec![7; 1024 * 1024])));
        assert_eq!(start.elapsed(), Duration::from_millis(81_920));
        assert_eq!(transport.targets(), vec![peer("slow"); 4]);
        transport.assert_queries_only(0);
    }

    #[tokio::test(start_paused = true)]
    async fn serving_peer_confirmed_disconnect_fails_over_after_reset_recovery() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("stalled"));
        registry.register_peer(TEST_CID, peer("good"));
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![
                (
                    "stalled",
                    Response::Chunks {
                        block: vec![0; 2 * HAVE_CHUNK_SIZE],
                        delay: Duration::from_millis(400),
                        limit: 1,
                    },
                ),
                ("good", Response::Immediate(b"payload".to_vec())),
            ],
        ));
        let disconnecting = transport.clone();
        let events = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            disconnecting.set_connection_state(&peer("stalled"), ConnectionState::Disconnected);
            events.sender_reset(&peer("stalled"), "");
        });
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(b"payload".to_vec())
        );
        assert_eq!(transport.targets(), vec![peer("stalled"), peer("good")]);
        assert_eq!(start.elapsed(), IDLE_TIMEOUT + Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn serving_peer_recovers_after_multiple_unreachable_idle_windows_without_reset() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("recovering"));
        registry.register_peer(TEST_CID, peer("fallback"));
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![
                ("recovering", Response::Silent),
                ("fallback", Response::Immediate(vec![9])),
            ],
        ));
        let incoming = registry.clone();
        let suspect = transport.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            incoming.fulfill_chunk_from(TEST_CID, &peer("recovering"), 0, 2, vec![7]);
            // Model topology sync removing the route during suspect grace.
            suspect.set_connection_state(&peer("recovering"), ConnectionState::Reconnecting);
            tokio::time::sleep(2 * IDLE_TIMEOUT + Duration::from_secs(1)).await;
            suspect.set_connection_state(&peer("recovering"), ConnectionState::Connected);
            incoming.fulfill_chunk_from(TEST_CID, &peer("recovering"), 1, 2, vec![8]);
        });
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(vec![7, 8])
        );
        assert_eq!(start.elapsed(), 2 * IDLE_TIMEOUT + Duration::from_secs(2));
        assert_eq!(transport.targets(), [peer("recovering")]);
        transport.assert_queries_only(0);
    }

    #[tokio::test(start_paused = true)]
    async fn connected_peer_with_only_prior_progress_still_fails_at_idle() {
        let registry = WantRegistry::new();
        let _other = registry.register_from(OTHER_CID, peer("silent"));
        let prior_progress = registry.progress(&peer("silent"));
        registry.fulfill_chunk_from(OTHER_CID, &peer("silent"), 0, 2, vec![1]);
        assert_eq!(*prior_progress.borrow(), 1);
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![("silent", Response::Silent)],
        ));
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .attempt(TEST_CID, &peer("silent"))
                .await,
            None
        );
        assert_eq!(start.elapsed(), IDLE_TIMEOUT);
        assert_eq!(transport.targets(), [peer("silent")]);
    }

    #[tokio::test(start_paused = true)]
    async fn silent_connected_serving_peer_is_bounded_by_fixed_cap() {
        let registry = WantRegistry::new();
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![(
                "silent",
                Response::Chunks {
                    block: vec![0; 2 * HAVE_CHUNK_SIZE],
                    delay: Duration::from_secs(1),
                    limit: 1,
                },
            )],
        ));
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport)
                .attempt(TEST_CID, &peer("silent"))
                .await,
            None
        );
        assert_eq!(start.elapsed(), ATTEMPT_CAP);
    }

    #[tokio::test(start_paused = true)]
    async fn serving_peer_without_confirmed_reset_waits_to_fixed_cap_despite_route_loss() {
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Reconnecting,
            ConnectionState::Failed,
        ] {
            let registry = WantRegistry::new();
            let transport = Arc::new(FakeTransport::new(
                &registry,
                vec![(
                    "remote",
                    Response::Chunks {
                        block: vec![0; 2 * HAVE_CHUNK_SIZE],
                        delay: Duration::from_secs(1),
                        limit: 1,
                    },
                )],
            ));
            let changing = transport.clone();
            tokio::spawn(async move {
                // Even after a route disappears, only confirmed reset or cap ends the wait.
                tokio::time::sleep(IDLE_TIMEOUT + Duration::from_secs(2)).await;
                changing.set_connection_state(&peer("remote"), state);
            });
            let start = Instant::now();
            assert_eq!(
                resolver(&registry, transport)
                    .attempt(TEST_CID, &peer("remote"))
                    .await,
                None
            );
            assert_eq!(start.elapsed(), ATTEMPT_CAP, "{state:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn serving_peer_connected_on_one_session_can_recover() {
        let registry = WantRegistry::new();
        let disconnected = Arc::new(FakeTransport::new(&registry, vec![]));
        disconnected.set_connection_state(&peer("remote"), ConnectionState::Reconnecting);
        let connected = Arc::new(FakeTransport::new(
            &registry,
            vec![("remote", Response::Silent)],
        ));
        let resolver = NetworkPeerResolver::new(
            Arc::new(FixedTransportSource(vec![disconnected, connected])),
            Arc::new(TestRuntime),
            registry.clone(),
            ResolverTimings::default(),
        );
        let incoming = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            incoming.fulfill_chunk_from(TEST_CID, &peer("remote"), 0, 2, vec![7]);
            tokio::time::sleep(IDLE_TIMEOUT + Duration::from_secs(1)).await;
            incoming.fulfill_chunk_from(TEST_CID, &peer("remote"), 1, 2, vec![8]);
        });
        assert_eq!(
            resolver.attempt(TEST_CID, &peer("remote")).await,
            Some(vec![7, 8])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unresponsive_peer_fails_over_to_a_live_peer() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("dead"));
        registry.register_peer(TEST_CID, peer("good"));
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![
                ("dead", Response::Silent),
                ("good", Response::Immediate(b"payload".to_vec())),
            ],
        ));
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(b"payload".to_vec())
        );
        assert_eq!(start.elapsed(), IDLE_TIMEOUT);
        assert_eq!(transport.targets(), vec![peer("dead"), peer("good")]);
    }

    async fn discovers_remote_provider(delay: Duration) {
        let registry = WantRegistry::new();
        let mut fake = FakeTransport::new(
            &registry,
            vec![("remote", Response::Immediate(b"remote block".to_vec()))],
        );
        fake.advertisements.push((peer("remote"), delay));
        let transport = Arc::new(fake);
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(b"remote block".to_vec())
        );
        assert_eq!(start.elapsed(), delay);
        assert_eq!(transport.targets(), vec![peer("remote")]);
        transport.assert_queries_only(1);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_at_900_ms_sends_only_targeted_want() {
        discovers_remote_provider(Duration::from_millis(900)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn first_discovery_reply_does_not_wait_for_broadcast_completion() {
        let registry = WantRegistry::new();
        let mut fake = FakeTransport::new(
            &registry,
            vec![("remote", Response::Immediate(b"payload".to_vec()))],
        );
        fake.advertisements
            .push((peer("remote"), Duration::from_millis(900)));
        fake.hang_after_query = true;
        let transport = Arc::new(fake);
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(b"payload".to_vec())
        );
        assert_eq!(start.elapsed(), Duration::from_millis(900));
        transport.assert_queries_only(1);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_accepts_synchronous_multihop_provider() {
        discovers_remote_provider(Duration::ZERO).await;
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_waits_even_without_initial_recipients() {
        discovers_remote_provider(Duration::from_millis(5)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn no_provider_returns_none_after_discovery_timeout() {
        let registry = WantRegistry::new();
        let transport = Arc::new(FakeTransport::new(&registry, vec![]));
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            None
        );
        assert_eq!(start.elapsed(), DISCOVERY_IDLE_TIMEOUT);
        assert!(transport.targets().is_empty());
        transport.assert_queries_only(1);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_waits_through_silence_only_with_an_outstanding_connected_sender() {
        for state in [
            ConnectionState::Connected,
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Reconnecting,
            ConnectionState::Failed,
        ] {
            let registry = WantRegistry::new();
            let _outstanding = registry.register_from(OTHER_CID, peer("serving"));
            let mut fake = FakeTransport::new(&registry, vec![]);
            fake.set_connection_state(&peer("serving"), state);
            fake.advertisements
                .push((peer("provider"), Duration::from_secs(25)));
            let transport = Arc::new(fake);
            let start = Instant::now();
            let providers = resolver(&registry, transport.clone())
                .discover(TEST_CID, &HashSet::new())
                .await;
            if state == ConnectionState::Connected {
                assert_eq!(providers.unwrap(), [peer("provider")]);
                assert_eq!(start.elapsed(), Duration::from_secs(25));
            } else {
                assert_eq!(
                    providers.unwrap_err(),
                    DiscoveryFailure::NoReachablePeers,
                    "{state:?}"
                );
                assert_eq!(start.elapsed(), DISCOVERY_IDLE_TIMEOUT, "{state:?}");
            }
            assert!(transport.targets().is_empty());
            transport.assert_queries_only(1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_waits_for_delayed_reply_without_open_wants() {
        for closed_registration in [false, true] {
            let registry = WantRegistry::new();
            if closed_registration {
                let abandoned = registry.register_from(OTHER_CID, peer("remote"));
                registry.fulfill_chunk_from(OTHER_CID, &peer("remote"), 0, 2, vec![1]);
                drop(abandoned);
            }
            let mut fake = FakeTransport::new(&registry, vec![]);
            fake.query_peers.push(peer("remote"));
            fake.advertisements
                .push((peer("remote"), Duration::from_secs(25)));
            let transport = Arc::new(fake);
            let start = Instant::now();
            assert_eq!(
                resolver(&registry, transport.clone())
                    .discover(TEST_CID, &HashSet::new())
                    .await
                    .unwrap(),
                [peer("remote")]
            );
            assert_eq!(start.elapsed(), Duration::from_secs(25));
            assert!(registry.outstanding_senders().is_empty());
            transport.assert_queries_only(3);
            assert!(transport.targets().is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn lost_initial_query_retries_after_fast_room_reconnect_without_replaying_want() {
        let registry = WantRegistry::new();
        let mut fake =
            FakeTransport::new(&registry, vec![("remote", Response::Immediate(vec![9]))]);
        fake.room = "main".into();
        fake.query_peers.push(peer("remote"));
        fake.advertise_after_round = 2; // first QUERY or HAVE_STATUS is lost
        fake.advertisements.push((peer("remote"), Duration::ZERO));
        let transport = Arc::new(fake);
        let reconnecting = transport.clone();
        let resets = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            reconnecting.set_connection_state(&peer("remote"), ConnectionState::Reconnecting);
            resets.room_reset("main");
            tokio::time::sleep(Duration::from_secs(3)).await;
            reconnecting.set_connection_state(&peer("remote"), ConnectionState::Connected);
        });
        let resolver = NetworkPeerResolver::new(
            Arc::new(RoomTransports(vec![("main".into(), transport.clone())])),
            Arc::new(TestRuntime),
            registry,
            ResolverTimings::default(),
        );
        let start = Instant::now();
        assert_eq!(
            resolver.resolve_block_result(TEST_CID).await.unwrap(),
            Some(vec![9])
        );
        assert_eq!(start.elapsed(), DISCOVERY_IDLE_TIMEOUT);
        transport.assert_queries_only(2);
        assert_eq!(transport.targets(), [peer("remote")]);
    }

    #[tokio::test(start_paused = true)]
    async fn silent_live_query_is_bounded_by_four_exponential_rounds_and_reports_timeout() {
        let registry = WantRegistry::new();
        let mut fake = FakeTransport::new(&registry, vec![]);
        fake.query_peers.push(peer("silent"));
        let transport = Arc::new(fake);
        let resolver = resolver(&registry, transport.clone());
        let start = Instant::now();
        let error = resolver.resolve_block_result(TEST_CID).await.unwrap_err();
        assert!(
            matches!(&error, MistError::Network(message) if message.contains("discovery query budget"))
        );
        assert_eq!(
            start.elapsed(),
            DISCOVERY_IDLE_TIMEOUT * ((1 << MAX_DISCOVERY_QUERY_ROUNDS) - 1)
        );
        assert!(start.elapsed() < ATTEMPT_CAP);
        transport.assert_queries_only(MAX_DISCOVERY_QUERY_ROUNDS as usize);
        assert!(transport.targets().is_empty());
        assert!(registry.outstanding_senders().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn queried_peer_reachable_only_in_an_unqueried_room_does_not_extend_discovery() {
        let registry = WantRegistry::new();
        let mut main = FakeTransport::new(&registry, vec![]);
        main.query_peers.push(peer("remote"));
        let main = Arc::new(main);
        // Same NodeId is reachable here but wasn't a recipient of this query.
        let other = Arc::new(FakeTransport::new(&registry, vec![]));
        let disconnecting = main.clone();
        let resets = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            disconnecting.set_connection_state(&peer("remote"), ConnectionState::Disconnected);
            resets.sender_reset(&peer("remote"), "main");
        });
        let resolver = NetworkPeerResolver::new(
            Arc::new(RoomTransports(vec![
                ("main".into(), main.clone()),
                ("other".into(), other.clone()),
            ])),
            Arc::new(TestRuntime),
            registry,
            ResolverTimings::default(),
        );
        let start = Instant::now();
        let error = resolver.resolve_block_result(TEST_CID).await.unwrap_err();
        assert!(
            matches!(&error, MistError::Network(message) if message.contains("No reachable peers"))
        );
        assert_eq!(start.elapsed(), DISCOVERY_IDLE_TIMEOUT);
        main.assert_queries_only(1);
        other.assert_queries_only(1);
    }

    #[tokio::test(start_paused = true)]
    async fn suspected_query_path_waits_without_spending_rounds_then_retries_on_recovery() {
        let registry = WantRegistry::new();
        let mut fake = FakeTransport::new(&registry, vec![]);
        fake.query_peers.push(peer("remote"));
        fake.advertise_after_round = 2;
        fake.advertisements.push((peer("remote"), Duration::ZERO));
        let transport = Arc::new(fake);
        let recovering = transport.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            recovering.set_connection_state(&peer("remote"), ConnectionState::Reconnecting);
            tokio::time::sleep(Duration::from_secs(89)).await;
            // Beyond the entire ordinary 75 s query budget, still one QUERY.
            recovering.assert_queries_only(1);
            recovering.set_connection_state(&peer("remote"), ConnectionState::Connected);
        });
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .discover(TEST_CID, &HashSet::new())
                .await
                .unwrap(),
            [peer("remote")]
        );
        assert!(start.elapsed() >= Duration::from_secs(90));
        assert!(start.elapsed() <= Duration::from_secs(90) + RECONNECT_POLL_INTERVAL);
        transport.assert_queries_only(2);
        assert!(transport.targets().is_empty());
        assert!(registry.outstanding_senders().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn unreachable_query_without_confirmed_reset_keeps_the_original_cap() {
        for state in [
            ConnectionState::Reconnecting,
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Failed,
        ] {
            let registry = WantRegistry::new();
            let mut fake = FakeTransport::new(&registry, vec![]);
            fake.query_peers.push(peer("remote"));
            let transport = Arc::new(fake);
            let suspect = transport.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                suspect.set_connection_state(&peer("remote"), state);
            });
            let start = Instant::now();
            assert_eq!(
                resolver(&registry, transport.clone())
                    .discover(TEST_CID, &HashSet::new())
                    .await
                    .unwrap_err(),
                DiscoveryFailure::TransportTimeout,
                "{state:?}"
            );
            assert_eq!(start.elapsed(), ATTEMPT_CAP);
            transport.assert_queries_only(1);
            assert!(registry.outstanding_senders().is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn query_reset_is_room_and_peer_scoped_and_all_paths_must_be_confirmed() {
        for whole_room in [false, true] {
            let registry = WantRegistry::new();
            let mut fake = FakeTransport::new(&registry, vec![]);
            fake.query_peers.extend([peer("first"), peer("second")]);
            let transport = Arc::new(fake);
            let disconnecting = transport.clone();
            let resets = registry.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                for node in ["first", "second"] {
                    disconnecting.set_connection_state(&peer(node), ConnectionState::Reconnecting);
                }
                resets.sender_reset(&peer("first"), "");
                resets.sender_reset(&peer("second"), "other-room");
                resets.sender_reset(&peer("unqueried"), "");
                tokio::time::sleep(Duration::from_secs(20)).await;
                if whole_room {
                    resets.room_reset("");
                } else {
                    resets.sender_reset(&peer("second"), "");
                }
            });
            let start = Instant::now();
            assert_eq!(
                resolver(&registry, transport.clone())
                    .discover(TEST_CID, &HashSet::new())
                    .await
                    .unwrap_err(),
                DiscoveryFailure::NoReachablePeers
            );
            assert!(start.elapsed() >= Duration::from_secs(21));
            assert!(start.elapsed() <= Duration::from_secs(21) + RECONNECT_POLL_INTERVAL);
            transport.assert_queries_only(1);
            assert!(registry.outstanding_senders().is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn suspected_query_on_actual_overlay_survives_routing_sync() {
        let registry = WantRegistry::new();
        let overlay = overlay_transport();
        let remote = peer("remote");
        overlay
            .router
            .sync_connection_states(&[(remote.clone(), ConnectionState::Connected)]);
        let suspect = overlay.clone();
        let replies = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            suspect
                .router
                .sync_connection_states(&[(remote.clone(), ConnectionState::Reconnecting)]);
            assert!(!suspect.is_reachable(&remote));
            tokio::time::sleep(Duration::from_secs(24)).await;
            // A late HAVE_STATUS is valid even before routing sync restores a route.
            replies.register_peer(TEST_CID, remote);
        });
        let resolver = NetworkPeerResolver::new(
            Arc::new(FixedTransportSource(vec![overlay])),
            Arc::new(TestRuntime),
            registry.clone(),
            ResolverTimings::default(),
        );
        let start = Instant::now();
        assert_eq!(
            resolver.discover(TEST_CID, &HashSet::new()).await.unwrap(),
            [peer("remote")]
        );
        assert_eq!(start.elapsed(), Duration::from_secs(25));
        assert!(registry.outstanding_senders().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn query_observes_confirmed_teardown_during_initial_enqueue() {
        let registry = WantRegistry::new();
        let mut fake = FakeTransport::new(&registry, vec![]);
        fake.query_peers.push(peer("remote"));
        fake.reset_during_query = true;
        let transport = Arc::new(fake);
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .discover(TEST_CID, &HashSet::new())
                .await
                .unwrap_err(),
            DiscoveryFailure::NoReachablePeers
        );
        assert_eq!(start.elapsed(), DISCOVERY_IDLE_TIMEOUT);
        transport.assert_queries_only(1);
    }

    #[tokio::test(start_paused = true)]
    async fn recovered_query_rearms_reset_observer_before_its_next_suspect_window() {
        let registry = WantRegistry::new();
        let mut fake = FakeTransport::new(&registry, vec![]);
        fake.query_peers.push(peer("remote"));
        fake.advertise_after_round = 3;
        fake.advertisements.push((peer("remote"), Duration::ZERO));
        let transport = Arc::new(fake);
        let recovering = transport.clone();
        let resets = registry.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            recovering.set_connection_state(&peer("remote"), ConnectionState::Reconnecting);
            resets.room_reset("");
            tokio::time::sleep(Duration::from_secs(3)).await;
            recovering.set_connection_state(&peer("remote"), ConnectionState::Connected);
            tokio::time::sleep(Duration::from_secs(2)).await;
            recovering.assert_queries_only(2);
            recovering.set_connection_state(&peer("remote"), ConnectionState::Reconnecting);
            tokio::time::sleep(Duration::from_secs(24)).await;
            recovering.assert_queries_only(2);
            recovering.set_connection_state(&peer("remote"), ConnectionState::Connected);
        });
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .discover(TEST_CID, &HashSet::new())
                .await
                .unwrap(),
            [peer("remote")]
        );
        assert!(start.elapsed() >= Duration::from_secs(30));
        assert!(start.elapsed() <= Duration::from_secs(30) + RECONNECT_POLL_INTERVAL);
        transport.assert_queries_only(3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_initial_query_without_wants_obeys_the_original_cap() {
        let registry = WantRegistry::new();
        let mut fake = FakeTransport::new(&registry, vec![]);
        fake.query_peers.push(peer("remote"));
        fake.hang_after_query = true;
        let transport = Arc::new(fake);
        let mut resolver = resolver(&registry, transport.clone());
        resolver.timings.attempt_cap = Duration::from_secs(12);
        let start = Instant::now();
        assert_eq!(
            resolver
                .discover(TEST_CID, &HashSet::new())
                .await
                .unwrap_err(),
            DiscoveryFailure::TransportTimeout
        );
        assert_eq!(start.elapsed(), Duration::from_secs(12));
        transport.assert_queries_only(1);
        assert!(transport.targets().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn outstanding_want_survives_query_budget_without_any_bulk_replay() {
        let registry = WantRegistry::new();
        let _outstanding = registry.register_from(OTHER_CID, peer("remote"));
        let mut fake = FakeTransport::new(&registry, vec![("remote", Response::Silent)]);
        fake.query_peers.push(peer("remote"));
        let transport = Arc::new(fake);
        transport
            .send(
                &peer("remote"),
                Bytes::from(super::super::protocol::build_want_message(OTHER_CID)),
                DeliveryMethod::ReliableOrdered,
            )
            .await
            .unwrap();
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .discover(TEST_CID, &HashSet::new())
                .await
                .unwrap_err(),
            DiscoveryFailure::TransportTimeout
        );
        assert_eq!(start.elapsed(), ATTEMPT_CAP);
        transport.assert_queries_only(MAX_DISCOVERY_QUERY_ROUNDS as usize);
        assert_eq!(transport.targets(), [peer("remote")]);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_with_silent_connected_outstanding_sender_has_a_fixed_cap() {
        let registry = WantRegistry::new();
        let _outstanding = registry.register_from(OTHER_CID, peer("remote"));
        let transport = Arc::new(FakeTransport::new(&registry, vec![]));
        let start = Instant::now();
        assert!(resolver(&registry, transport)
            .discover(TEST_CID, &HashSet::new())
            .await
            .is_err());
        assert_eq!(start.elapsed(), ATTEMPT_CAP);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_rechecks_outstanding_senders_after_the_first_idle_window() {
        let registry = WantRegistry::new();
        let outstanding = registry.register_from(OTHER_CID, peer("remote"));
        let transport = Arc::new(FakeTransport::new(&registry, vec![]));
        let disconnecting = transport.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DISCOVERY_IDLE_TIMEOUT + Duration::from_secs(1)).await;
            disconnecting.set_connection_state(&peer("remote"), ConnectionState::Reconnecting);
        });
        let start = Instant::now();
        assert!(resolver(&registry, transport)
            .discover(TEST_CID, &HashSet::new())
            .await
            .is_err());
        assert_eq!(start.elapsed(), 2 * DISCOVERY_IDLE_TIMEOUT);
        drop(outstanding);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_survives_a_25_second_reply_delay_with_other_want_progress() {
        let registry = WantRegistry::new();
        registry.register_peer(OTHER_CID, peer("remote"));
        let mut fake = FakeTransport::new(
            &registry,
            vec![(
                "remote",
                Response::Chunks {
                    block: vec![7; 1024 * 1024],
                    delay: Duration::from_millis(500),
                    limit: usize::MAX,
                },
            )],
        );
        fake.responses_by_cid
            .insert(LATER_CID.into(), Response::Immediate(vec![9]));
        fake.advertisements
            .push((peer("remote"), Duration::from_secs(25)));
        let transport = Arc::new(fake);
        let resolver = resolver(&registry, transport.clone());
        let start = Instant::now();
        let (other, later) = tokio::join!(
            resolver.resolve_block(OTHER_CID),
            resolver.resolve_block(LATER_CID)
        );
        assert_eq!(other, Some(vec![7; 1024 * 1024]));
        assert_eq!(later, Some(vec![9]));
        let sends = transport.sends.lock().unwrap();
        assert_eq!(sends[1].1, LATER_CID);
        assert_eq!(sends[1].2 - start, Duration::from_secs(25));
        transport.assert_queries_only(1);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_idle_window_starts_again_after_the_last_progress() {
        let registry = WantRegistry::new();
        let _other = registry.register_from(OTHER_CID, peer("remote"));
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![(
                "remote",
                Response::Chunks {
                    block: vec![0; 4 * HAVE_CHUNK_SIZE],
                    delay: Duration::from_secs(1),
                    limit: 3,
                },
            )],
        ));
        let disconnecting = transport.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(3500)).await;
            disconnecting.set_connection_state(&peer("remote"), ConnectionState::Disconnected);
        });
        transport
            .send(
                &peer("remote"),
                Bytes::from([&[MSG_WANT], OTHER_CID.as_bytes()].concat()),
                DeliveryMethod::ReliableOrdered,
            )
            .await
            .unwrap();
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport)
                .resolve_block(MISSING_CID)
                .await,
            None
        );
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(3) + DISCOVERY_IDLE_TIMEOUT
        );
    }

    #[tokio::test(start_paused = true)]
    async fn trickling_progress_cannot_extend_discovery_cap_even_during_broadcast() {
        let registry = WantRegistry::new();
        let _other = registry.register_from(OTHER_CID, peer("remote"));
        let progress = registry.any_progress();
        let mut fake = FakeTransport::new(
            &registry,
            vec![(
                "remote",
                Response::Chunks {
                    block: vec![0; 1024 * 1024],
                    delay: Duration::from_millis(500),
                    limit: usize::MAX,
                },
            )],
        );
        fake.hang_after_query = true;
        let transport = Arc::new(fake);
        transport
            .send(
                &peer("remote"),
                Bytes::from([&[MSG_WANT], OTHER_CID.as_bytes()].concat()),
                DeliveryMethod::ReliableOrdered,
            )
            .await
            .unwrap();
        let mut resolver = resolver(&registry, transport.clone());
        resolver.timings.attempt_cap = Duration::from_secs(12);
        let start = Instant::now();
        assert_eq!(resolver.resolve_block(MISSING_CID).await, None);
        assert_eq!(start.elapsed(), resolver.timings.attempt_cap);
        assert!(*progress.borrow() >= 23);
        transport.assert_queries_only(1);
    }

    #[tokio::test(start_paused = true)]
    async fn prefetch_broadcasts_uncached_cids_on_each_transport_without_waiting_for_replies() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CIDS[0], peer("remote"));
        let mut fake =
            FakeTransport::new(&registry, vec![("remote", Response::Immediate(vec![3]))]);
        fake.advertisements
            .push((peer("remote"), Duration::from_millis(900)));
        let transport = Arc::new(fake);
        let second = Arc::new(FakeTransport::new(&registry, vec![]));
        let resolver = NetworkPeerResolver::new(
            Arc::new(FixedTransportSource(vec![
                transport.clone(),
                second.clone(),
            ])),
            Arc::new(TestRuntime),
            registry.clone(),
            ResolverTimings::default(),
        );
        let start = Instant::now();
        resolver
            .prefetch_providers(&[
                TEST_CIDS[0].into(),
                TEST_CIDS[1].into(),
                TEST_CIDS[2].into(),
                TEST_CIDS[2].into(),
            ])
            .await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert!(registry.get_peers(TEST_CIDS[2]).is_empty());
        for transport in [&transport, &second] {
            let queries: Vec<_> = transport
                .broadcasts
                .lock()
                .unwrap()
                .iter()
                .map(|data| parse_query_message(data).unwrap())
                .collect();
            assert_eq!(queries, [TEST_CIDS[1], TEST_CIDS[2]]);
        }
        registry.register_peer_notifier(TEST_CIDS[2]).await.unwrap();
        assert_eq!(start.elapsed(), Duration::from_millis(900));
        assert_eq!(registry.get_peers(TEST_CIDS[2]), [peer("remote")]);
        assert_eq!(resolver.resolve_block(TEST_CIDS[2]).await, Some(vec![3]));
        assert_eq!(transport.targets(), [peer("remote")]);
        transport.assert_queries_only(2);
        second.assert_queries_only(2);
    }

    #[tokio::test(start_paused = true)]
    async fn prefetch_bounds_the_number_of_input_cids() {
        let registry = WantRegistry::new();
        let transport = Arc::new(FakeTransport::new(&registry, vec![]));
        let cids: Vec<_> = (0..MAX_PREFETCH_CIDS + 4)
            .map(|i| {
                crate::storage::cid::compute_cid(
                    format!("cid-{i}").as_bytes(),
                    crate::storage::cid::MULTICODEC_RAW,
                )
            })
            .collect();
        resolver(&registry, transport.clone())
            .prefetch_providers(&cids)
            .await;
        transport.assert_queries_only(MAX_PREFETCH_CIDS);
    }

    #[tokio::test(start_paused = true)]
    async fn five_blocks_with_four_in_flight_survive_ordered_discovery_behind_bulk_data() {
        let registry = WantRegistry::new();
        let mut fake = FakeTransport::new(&registry, vec![("remote", slow_block())]);
        fake.ordered_advertisements = true;
        fake.advertisements.push((peer("remote"), Duration::ZERO));
        fake.responses_by_cid.insert(
            TEST_CIDS[4].into(),
            Response::Chunks {
                block: vec![9],
                delay: Duration::from_millis(320),
                limit: usize::MAX,
            },
        );
        let transport = Arc::new(fake);
        let resolver = resolver(&registry, transport.clone());
        let start = Instant::now();
        // Deliberately skip prefetch: correctness must hold for callers that
        // discover the fifth block only after a slot in the window opens.
        let blocks: Vec<_> = stream::iter([
            TEST_CIDS[0],
            TEST_CIDS[1],
            TEST_CIDS[2],
            TEST_CIDS[3],
            TEST_CIDS[4],
        ])
        .map(|cid| resolver.resolve_block(cid))
        .buffered(4)
        .collect()
        .await;
        assert!(blocks[..4]
            .iter()
            .all(|block| *block == Some(vec![7; 1024 * 1024])));
        assert_eq!(blocks[4], Some(vec![9]));
        assert_eq!(transport.targets(), vec![peer("remote"); 5]);
        transport.assert_queries_only(5);
        let sends = transport.sends.lock().unwrap();
        // The fifth QUERY's reply waits for all three remaining MiB blocks;
        // it is not delivered immediately by the fake's advertisement path.
        assert_eq!(sends[4].1, TEST_CIDS[4]);
        assert_eq!(sends[4].2 - start, Duration::from_millis(81_920));
        assert_eq!(start.elapsed(), Duration::from_millis(82_240));
    }

    #[tokio::test(start_paused = true)]
    async fn trickling_peer_cannot_extend_attempt_cap() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("trickle"));
        registry.register_peer(TEST_CID, peer("good"));
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![
                (
                    "trickle",
                    Response::Chunks {
                        block: vec![0; 1024 * 1024],
                        delay: Duration::from_secs(9),
                        limit: usize::MAX,
                    },
                ),
                ("good", Response::Immediate(b"payload".to_vec())),
            ],
        ));
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(b"payload".to_vec())
        );
        assert_eq!(start.elapsed(), ATTEMPT_CAP);
        assert_eq!(transport.targets(), vec![peer("trickle"), peer("good")]);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_sends_skip_peer_without_waiting_for_idle() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("unreachable"));
        registry.register_peer(TEST_CID, peer("good"));
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![
                ("unreachable", Response::Reject),
                ("good", Response::Immediate(b"payload".to_vec())),
            ],
        ));
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(b"payload".to_vec())
        );
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(transport.targets(), vec![peer("unreachable"), peer("good")]);
    }

    #[tokio::test(start_paused = true)]
    async fn chunk_wants_are_distributed_across_multiple_peers() {
        let registry = WantRegistry::new();
        let cids = [
            TEST_CIDS[0],
            TEST_CIDS[1],
            TEST_CIDS[2],
            TEST_CIDS[3],
            TEST_CIDS[4],
            TEST_CIDS[5],
        ];
        for cid in cids {
            for id in ["peer-a", "peer-b", "peer-c"] {
                registry.register_peer(cid, peer(id));
            }
        }
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![
                ("peer-a", Response::Silent),
                ("peer-b", Response::Silent),
                ("peer-c", Response::Silent),
            ],
        ));
        let resolver = resolver(&registry, transport.clone());
        let results =
            futures_util::future::join_all(cids.iter().map(|cid| resolver.resolve_block(cid)))
                .await;
        assert!(results.into_iter().all(|result| result.is_none()));
        let mut first = BTreeMap::new();
        for (node, cid, _) in transport.sends.lock().unwrap().iter() {
            first.entry(cid.clone()).or_insert_with(|| node.0.clone());
        }
        assert_eq!(first.len(), cids.len());
        assert_eq!(first.values().collect::<BTreeSet<_>>().len(), 3);
        for cid in cids {
            let targets: Vec<_> = transport
                .sends
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, sent_cid, _)| sent_cid == cid)
                .map(|(node, _, _)| node.clone())
                .collect();
            assert_eq!(targets.len(), 3);
            assert_eq!(targets.into_iter().collect::<HashSet<_>>().len(), 3);
        }
        transport.assert_queries_only(6);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_cached_providers_are_not_retried_after_discovery() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("dead"));
        let mut fake = FakeTransport::new(
            &registry,
            vec![
                ("dead", Response::Reject),
                ("new", Response::Immediate(b"payload".to_vec())),
            ],
        );
        fake.advertisements = vec![
            (peer("dead"), Duration::from_millis(100)),
            (peer("new"), Duration::from_millis(900)),
        ];
        let transport = Arc::new(fake);
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(b"payload".to_vec())
        );
        assert_eq!(start.elapsed(), Duration::from_millis(900));
        assert_eq!(transport.targets(), vec![peer("dead"), peer("new")]);
        transport.assert_queries_only(1);
    }

    #[tokio::test(start_paused = true)]
    async fn attempt_count_is_bounded() {
        let registry = WantRegistry::new();
        for id in ["a", "b", "c", "d", "e"] {
            registry.register_peer(TEST_CID, peer(id));
        }
        let transport = Arc::new(FakeTransport::new(&registry, vec![]));
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            None
        );
        assert_eq!(
            transport.targets(),
            vec![peer("a"), peer("b"), peer("c"), peer("d")]
        );
        transport.assert_queries_only(0);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_replies_during_an_attempt_remain_candidates() {
        let registry = WantRegistry::new();
        let mut fake = FakeTransport::new(
            &registry,
            vec![
                ("dead", Response::Silent),
                ("good", Response::Immediate(b"payload".to_vec())),
            ],
        );
        fake.advertisements = vec![
            (peer("dead"), Duration::ZERO),
            (peer("good"), Duration::from_secs(1)),
        ];
        let transport = Arc::new(fake);
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport.clone())
                .resolve_block(TEST_CID)
                .await,
            Some(b"payload".to_vec())
        );
        assert_eq!(start.elapsed(), IDLE_TIMEOUT);
        assert_eq!(transport.targets(), vec![peer("dead"), peer("good")]);
        transport.assert_queries_only(1);
    }

    #[tokio::test(start_paused = true)]
    async fn successful_send_on_any_session_is_sufficient() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("remote"));
        let rejecting = Arc::new(FakeTransport::new(&registry, vec![]));
        let good = Arc::new(FakeTransport::new(
            &registry,
            vec![("remote", Response::Immediate(b"payload".to_vec()))],
        ));
        let resolver = NetworkPeerResolver::new(
            Arc::new(FixedTransportSource(vec![rejecting.clone(), good.clone()])),
            Arc::new(TestRuntime),
            registry,
            ResolverTimings::default(),
        );
        assert_eq!(
            resolver.resolve_block(TEST_CID).await,
            Some(b"payload".to_vec())
        );
        assert_eq!(rejecting.targets(), vec![peer("remote")]);
        assert_eq!(good.targets(), vec![peer("remote")]);
    }

    #[tokio::test(start_paused = true)]
    async fn attempt_cap_also_bounds_a_hanging_send() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("hanging"));
        registry.register_peer(TEST_CID, peer("good"));
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![
                ("hanging", Response::HangingSend),
                ("good", Response::Immediate(b"payload".to_vec())),
            ],
        ));
        let start = Instant::now();
        assert_eq!(
            resolver(&registry, transport).resolve_block(TEST_CID).await,
            Some(b"payload".to_vec())
        );
        assert_eq!(start.elapsed(), ATTEMPT_CAP);
    }

    #[tokio::test(start_paused = true)]
    async fn completed_block_does_not_wait_for_other_sessions_to_send() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("remote"));
        let good = Arc::new(FakeTransport::new(
            &registry,
            vec![("remote", Response::Immediate(b"payload".to_vec()))],
        ));
        let hanging = Arc::new(FakeTransport::new(
            &registry,
            vec![("remote", Response::HangingSend)],
        ));
        let resolver = NetworkPeerResolver::new(
            Arc::new(FixedTransportSource(vec![good, hanging])),
            Arc::new(TestRuntime),
            registry,
            ResolverTimings::default(),
        );
        let start = Instant::now();
        assert_eq!(
            resolver.resolve_block(TEST_CID).await,
            Some(b"payload".to_vec())
        );
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_resolve_prunes_its_waiter_and_progress() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("silent"));
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![("silent", Response::Silent)],
        ));
        let resolver = resolver(&registry, transport);
        let mut resolve = Box::pin(resolver.resolve_block_result(TEST_CID));
        assert!(futures_util::poll!(resolve.as_mut()).is_pending());
        let progress = registry.progress(&peer("silent"));
        assert!(progress.has_changed().is_ok());
        drop(resolve);
        assert!(progress.has_changed().is_err());
    }
    fn transfer_resolver(
        registry: &WantRegistry,
        transport: Arc<FakeTransport>,
        transfer: StorageTransferConfig,
    ) -> NetworkPeerResolver {
        NetworkPeerResolver::new_with_transfer(
            Arc::new(FixedTransportSource(vec![transport])),
            Arc::new(TestRuntime),
            registry.clone(),
            ResolverTimings::default(),
            transfer,
        )
    }

    #[derive(Default)]
    struct FetchStore(Mutex<HashMap<String, Vec<u8>>>);

    #[async_trait]
    impl crate::storage::BlockStore for FetchStore {
        async fn load_block(&self, cid: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(cid).cloned())
        }
        async fn store_block(&self, cid: &str, data: &[u8]) -> Result<()> {
            self.0.lock().unwrap().insert(cid.into(), data.to_vec());
            Ok(())
        }
        async fn delete_block(&self, cid: &str) -> Result<()> {
            self.0.lock().unwrap().remove(cid);
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn coalescing_concurrent_gets_sends_one_want() {
        use crate::storage::cid::{compute_cid, MULTICODEC_DAG_CBOR};
        use crate::storage::engine::{SpatialPolicy, StorageEngine};
        use crate::storage::types::FileManifest;
        let registry = WantRegistry::new();
        let block = serde_cbor::to_vec(&FileManifest {
            name: "empty".into(),
            size: 0,
            chunks: vec![],
        })
        .unwrap();
        let cid = compute_cid(&block, MULTICODEC_DAG_CBOR);
        registry.register_peer(&cid, peer("a"));
        let transport = Arc::new(FakeTransport::new(&registry, vec![("a", Response::Silent)]));
        let transfer = StorageTransferConfig {
            coalesce_fetches: true,
            ..Default::default()
        };
        let engine = StorageEngine::new_with_transfer(
            FetchStore::default(),
            transfer_resolver(&registry, transport.clone(), transfer.clone()),
            1024,
            None,
            SpatialPolicy::default(),
            transfer,
        );
        let mut gets: Vec<_> = (0..8).map(|_| Box::pin(engine.get(&cid))).collect();
        for get in &mut gets {
            assert!(futures_util::poll!(get.as_mut()).is_pending());
        }
        assert_eq!(transport.targets(), vec![peer("a")]);
        registry.fulfill_from(&cid, &peer("a"), block);
        for get in gets {
            assert_eq!(get.await.unwrap(), Vec::<u8>::new());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn coalescing_subscriber_cancel_transfers_driving_and_last_drop_cancels() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("a"));
        let transport = Arc::new(FakeTransport::new(&registry, vec![("a", Response::Silent)]));
        let resolver = transfer_resolver(
            &registry,
            transport.clone(),
            StorageTransferConfig {
                coalesce_fetches: true,
                max_fetches_per_provider: 1,
                ..Default::default()
            },
        );
        let mut first = Box::pin(resolver.resolve_block_result(TEST_CID));
        let mut second = Box::pin(resolver.resolve_block_result(TEST_CID));
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        drop(first);
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        assert_eq!(transport.targets().len(), 1);
        registry.fulfill_from(TEST_CID, &peer("a"), b"ok".to_vec());
        assert_eq!(second.await.unwrap(), Some(b"ok".to_vec()));

        let mut first = Box::pin(resolver.resolve_block_result(TEST_CID));
        let mut second = Box::pin(resolver.resolve_block_result(TEST_CID));
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        let semaphore = resolver.provider_limit(&peer("a")).unwrap();
        assert_eq!(semaphore.available_permits(), 0);
        drop(first);
        drop(second);
        assert_eq!(semaphore.available_permits(), 1);
        assert!(registry.outstanding_senders().is_empty());
        let mut again = Box::pin(resolver.resolve_block_result(TEST_CID));
        assert!(futures_util::poll!(again.as_mut()).is_pending());
        assert_eq!(transport.targets().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn coalescing_errors_are_shared_and_room_sets_are_isolated() {
        let registry = WantRegistry::new();
        let transport = Arc::new(FakeTransport::new(&registry, vec![]));
        let resolver = transfer_resolver(
            &registry,
            transport.clone(),
            StorageTransferConfig {
                coalesce_fetches: true,
                ..Default::default()
            },
        );
        let mut first = Box::pin(resolver.resolve_block_result(TEST_CID));
        let mut second = Box::pin(resolver.resolve_block_result(TEST_CID));
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        tokio::time::advance(DISCOVERY_IDLE_TIMEOUT).await;
        let a = first.await.unwrap_err().to_string();
        let b = second.await.unwrap_err().to_string();
        assert_eq!(a, b);
        assert!(a.contains("No reachable peers"));
        transport.assert_queries_only(1);

        registry.register_peer(TEST_CID, peer("a"));
        let transport = Arc::new(FakeTransport::new(&registry, vec![("a", Response::Silent)]));
        let mut resolver = transfer_resolver(
            &registry,
            transport.clone(),
            StorageTransferConfig {
                coalesce_fetches: true,
                ..Default::default()
            },
        );
        resolver.transports = Arc::new(RoomTransports(vec![("room-a".into(), transport.clone())]));
        let mut other_room = resolver.clone();
        other_room.transports =
            Arc::new(RoomTransports(vec![("room-b".into(), transport.clone())]));
        let mut a = Box::pin(resolver.resolve_block_result(TEST_CID));
        let mut b = Box::pin(other_room.resolve_block_result(TEST_CID));
        assert!(futures_util::poll!(a.as_mut()).is_pending());
        assert!(futures_util::poll!(b.as_mut()).is_pending());
        assert_eq!(transport.targets().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn provider_cap_prefers_free_provider_and_never_exceeds_limit() {
        for cap in [1, 2] {
            let registry = WantRegistry::new();
            for cid in TEST_CIDS {
                registry.register_peer(cid, peer("a"));
                registry.register_peer(cid, peer("b"));
            }
            let transport = Arc::new(FakeTransport::new(
                &registry,
                vec![("a", Response::Silent), ("b", Response::Silent)],
            ));
            let resolver = transfer_resolver(
                &registry,
                transport.clone(),
                StorageTransferConfig {
                    max_fetches_per_provider: cap,
                    ..Default::default()
                },
            );
            // Force round-robin to pick the saturated first provider on every call.
            let mut fetches = Vec::new();
            for cid in TEST_CIDS {
                resolver.next_peer.store(0, Ordering::Relaxed);
                let mut fetch = Box::pin(resolver.resolve_block_result(cid));
                assert!(futures_util::poll!(fetch.as_mut()).is_pending());
                fetches.push(Some(fetch));
            }
            let mut served = 0;
            while served < TEST_CIDS.len() {
                let sends = transport.sends.lock().unwrap().clone();
                let batch = &sends[served..];
                assert!(!batch.is_empty());
                let mut providers = HashMap::new();
                for (provider, cid, _) in batch {
                    let count = providers.entry(provider.clone()).or_insert(0);
                    *count += 1;
                    assert!(*count <= cap, "provider exceeded its cap");
                    registry.fulfill_from(cid, provider, b"ok".to_vec());
                }
                if served == 0 {
                    assert_eq!(providers.len(), 2);
                    assert_eq!(batch.len(), 2 * cap as usize);
                }
                served = sends.len();
                for fetch in &mut fetches {
                    if let Some(future) = fetch {
                        if let std::task::Poll::Ready(result) = futures_util::poll!(future.as_mut())
                        {
                            assert_eq!(result.unwrap(), Some(b"ok".to_vec()));
                            *fetch = None;
                        }
                    }
                }
            }
            assert!(fetches.iter().all(Option::is_none));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn provider_permit_wait_excludes_idle_but_counts_toward_fixed_cap() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("a"));
        let transport = Arc::new(FakeTransport::new(&registry, vec![("a", Response::Silent)]));
        let resolver = transfer_resolver(
            &registry,
            transport.clone(),
            StorageTransferConfig {
                max_fetches_per_provider: 1,
                ..Default::default()
            },
        );
        let semaphore = resolver.provider_limit(&peer("a")).unwrap();
        let held = semaphore.clone().acquire_owned().await.unwrap();
        let mut fetch = Box::pin(resolver.resolve_block_result(TEST_CID));
        assert!(futures_util::poll!(fetch.as_mut()).is_pending());
        tokio::time::advance(IDLE_TIMEOUT * 2).await;
        assert!(futures_util::poll!(fetch.as_mut()).is_pending());
        assert!(transport.targets().is_empty());
        drop(held);
        assert!(futures_util::poll!(fetch.as_mut()).is_pending());
        assert_eq!(transport.targets().len(), 1);
        tokio::time::advance(IDLE_TIMEOUT - Duration::from_secs(1)).await;
        assert!(futures_util::poll!(fetch.as_mut()).is_pending());
        registry.fulfill_from(TEST_CID, &peer("a"), b"ok".to_vec());
        assert_eq!(fetch.await.unwrap(), Some(b"ok".to_vec()));

        let held = semaphore.clone().acquire_owned().await.unwrap();
        let target = peer("a");
        let mut attempt = Box::pin(resolver.attempt(TEST_CID, &target));
        assert!(futures_util::poll!(attempt.as_mut()).is_pending());
        tokio::time::advance(ATTEMPT_CAP).await;
        assert_eq!(attempt.await, None);
        assert_eq!(transport.targets().len(), 1);
        drop(held);
    }
    #[tokio::test(start_paused = true)]
    async fn coalescing_map_is_bounded_and_cancelled_slots_are_reusable() {
        let registry = WantRegistry::new();
        let transport = Arc::new(FakeTransport::new(&registry, vec![]));
        let resolver = transfer_resolver(
            &registry,
            transport,
            StorageTransferConfig {
                coalesce_fetches: true,
                ..Default::default()
            },
        );
        let cids: Vec<_> = (0..MAX_IN_FLIGHT_FETCHES + 1)
            .map(|index| {
                crate::storage::cid::compute_cid(
                    &index.to_le_bytes(),
                    crate::storage::cid::MULTICODEC_RAW,
                )
            })
            .collect();
        let mut fetches = Vec::new();
        for cid in &cids[..MAX_IN_FLIGHT_FETCHES] {
            let mut fetch = Box::pin(resolver.resolve_block_result(cid));
            assert!(futures_util::poll!(fetch.as_mut()).is_pending());
            fetches.push(fetch);
        }
        assert_eq!(
            resolver.in_flight.lock().unwrap().len(),
            MAX_IN_FLIGHT_FETCHES
        );
        let error = resolver
            .resolve_block_result(&cids[MAX_IN_FLIGHT_FETCHES])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Too many in-flight"));
        // Even a full map permits another subscriber to an existing key.
        let mut subscriber = Box::pin(resolver.resolve_block_result(&cids[0]));
        assert!(futures_util::poll!(subscriber.as_mut()).is_pending());
        drop(subscriber);
        fetches.pop();
        let mut replacement = Box::pin(resolver.resolve_block_result(&cids[MAX_IN_FLIGHT_FETCHES]));
        assert!(futures_util::poll!(replacement.as_mut()).is_pending());
        assert_eq!(
            resolver.in_flight.lock().unwrap().len(),
            MAX_IN_FLIGHT_FETCHES
        );
    }

    #[tokio::test(start_paused = true)]
    async fn default_flags_keep_same_cid_fetches_independent_and_provider_unlimited() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("a"));
        let transport = Arc::new(FakeTransport::new(&registry, vec![("a", Response::Silent)]));
        let resolver = resolver(&registry, transport.clone());
        let mut fetches: Vec<_> = (0..8)
            .map(|_| Box::pin(resolver.resolve_block_result(TEST_CID)))
            .collect();
        for fetch in &mut fetches {
            assert!(futures_util::poll!(fetch.as_mut()).is_pending());
        }
        assert_eq!(transport.targets().len(), 8);
        registry.fulfill_from(TEST_CID, &peer("a"), b"ok".to_vec());
        for fetch in fetches {
            assert_eq!(fetch.await.unwrap(), Some(b"ok".to_vec()));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn provider_permits_release_on_idle_failover_and_cancel() {
        let registry = WantRegistry::new();
        registry.register_peer(TEST_CID, peer("a"));
        registry.register_peer(TEST_CID, peer("b"));
        let transport = Arc::new(FakeTransport::new(
            &registry,
            vec![
                ("a", Response::Silent),
                ("b", Response::Immediate(b"ok".to_vec())),
            ],
        ));
        let resolver = transfer_resolver(
            &registry,
            transport.clone(),
            StorageTransferConfig {
                max_fetches_per_provider: 1,
                ..Default::default()
            },
        );
        let a = resolver.provider_limit(&peer("a")).unwrap();
        let b = resolver.provider_limit(&peer("b")).unwrap();
        let mut fetch = Box::pin(resolver.resolve_block_result(TEST_CID));
        assert!(futures_util::poll!(fetch.as_mut()).is_pending());
        assert_eq!(a.available_permits(), 0);
        tokio::time::advance(IDLE_TIMEOUT).await;
        assert_eq!(fetch.await.unwrap(), Some(b"ok".to_vec()));
        assert_eq!(transport.targets(), vec![peer("a"), peer("b")]);
        assert_eq!(a.available_permits(), 1);
        assert_eq!(b.available_permits(), 1);
        resolver.next_peer.store(0, Ordering::Relaxed);
        let mut fetch = Box::pin(resolver.resolve_block_result(TEST_CID));
        assert!(futures_util::poll!(fetch.as_mut()).is_pending());
        drop(fetch);
        assert_eq!(a.available_permits(), 1);
        assert!(registry.outstanding_senders().is_empty());
    }
}
