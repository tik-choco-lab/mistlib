use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
pub use crate::events::*;
pub use crate::logging::*;
use crate::signaling::BootstrapSignaler;
use crate::transports::WebRtcTransport;
pub use mistlib_core::layers::L1Transport;
use mistlib_core::overlay::node_store::NodeStore;
use mistlib_core::overlay::OverlayRouter;
use mistlib_core::signaling::{Signaler, SignalingHandler};
use mistlib_core::transport::Transport;
use mistlib_core::types::NodeId;

mod action;
mod aoi;
mod background;
mod network;
mod run;
mod stats;

/// The full per-room stack (SPEC-15): a session is created by `join_room` and
/// torn down by `leave_room`/`leave_room_id`. All sessions share the engine's
/// single `self_id`, `Config`, and Tokio `Runtime` (see `MistEngine`); every
/// room-specific piece -- signaling, WebRTC transport, overlay/DNVE3, node
/// store, background loops -- lives here instead.
pub struct SessionCtx {
    pub(crate) room_id: String,
    pub(crate) transport: Arc<dyn Transport>,
    pub(crate) webrtc_transport: Option<Arc<WebRtcTransport>>,
    /// WebSocket経由で届いたシグナリング（SDP/ICE）の処理先
    pub(crate) ws_signaling_handler: Arc<dyn SignalingHandler>,
    /// WebRTC P2P中継で届いたシグナリングの処理先（中継不使用時はNone）
    pub(crate) p2p_signaling_handler: Option<Arc<dyn SignalingHandler>>,
    pub(crate) signaling_dispatch: Option<Arc<dyn Signaler>>,
    pub(crate) bootstrap_signaler: Option<Arc<BootstrapSignaler>>,
    pub(crate) l1_transport: Option<Arc<dyn L1Transport>>,
    pub(crate) l1_notifier: Option<Arc<dyn mistlib_core::layers::L1Notifier>>,
    pub(crate) overlay: Option<Arc<OverlayRouter>>,
    pub(crate) node_store: Arc<StdMutex<NodeStore>>,
    pub(crate) aoi_nodes: Arc<StdMutex<HashSet<NodeId>>>,
    pub(crate) had_connected_peers: AtomicBool,
    pub(crate) all_connections_lost_dispatched: AtomicBool,
    /// Cancels this session's background loops (AOI/neighbor + overlay tick,
    /// the network event pump, and the signaling loop) without touching any
    /// other active session. Cancelled once, from `leave_room`/`leave_room_id`.
    pub(crate) cancel: CancellationToken,
}

impl SessionCtx {
    pub(crate) fn ensure_node_registered(&self, node_id: &NodeId) {
        if node_id.is_server() {
            return;
        }
        let mut store = self.node_store.lock().unwrap();
        if !store.nodes.contains_key(node_id) {
            store.update_node_position(
                node_id.clone(),
                mistlib_core::overlay::dnve3::Vector3::zero(),
            );
        }
    }

    pub(crate) fn touch_node(&self, node_id: &NodeId) {
        if node_id.is_server() {
            return;
        }
        self.node_store.lock().unwrap().touch_node(node_id);
    }

    pub(crate) fn reset_connection_loss_tracking(&self) {
        self.had_connected_peers
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.all_connections_lost_dispatched
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

pub struct MistEngine {
    pub(crate) global_callback: Arc<StdMutex<Option<EventCallback>>>,
    pub(crate) global_callback_v2: Arc<StdMutex<Option<EventCallbackV2>>>,
    pub(crate) rust_event_callback: Arc<StdMutex<Option<RustEventCallback>>>,
    pub(crate) event_dispatch_tx: mpsc::UnboundedSender<(u32, String, NodeId, Vec<u8>)>,
    pub(crate) log_callback: StdMutex<Option<LogCallback>>,
    /// Insertion-ordered registry of active per-room sessions. Join order is
    /// what the roomless legacy FFI surface (send_message/update_position/
    /// on_connected/...) falls back to when it isn't scoped to a room -- see
    /// `primary_session`/`resolve_unicast_session` below and SPEC-15 rules 5-7.
    /// `std::sync::RwLock`, not `tokio::sync::RwLock`: `handle_action_in_room`
    /// (`engine/action.rs`) needs a synchronous, non-`.await` read of this
    /// registry so an `OverlayAction::SendMessage` can be enqueued into its
    /// target peer's ordered send queue inline, in the exact call order
    /// overlay seq numbers were stamped in -- see that function's doc comment.
    /// Every access here is a brief, uncontended read/write with no other
    /// `.await` inside the critical section (verified across every method
    /// below), so a blocking std lock is safe to take from async code, the
    /// same pattern already used for `SessionCtx::node_store`/`aoi_nodes` and
    /// most of `WebRtcTransport`'s own per-connection state.
    pub(crate) sessions: StdRwLock<Vec<(String, Arc<SessionCtx>)>>,
    pub(crate) config: StdMutex<Config>,
    pub(crate) runtime: Runtime,
    pub(crate) self_id: StdMutex<NodeId>,
    pub(crate) l0: Arc<crate::layers::native_l0::NativeL0>,
    /// Set once `init`/`init_with_config` has run. `join_room` before that is
    /// a no-op (there is no `self_id`/`Config` worth building a session from
    /// yet) -- mirrors the old `EngineState::Idle` guard.
    pub(crate) initialized: AtomicBool,
}

impl Default for MistEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MistEngine {
    pub fn new() -> Self {
        let global_callback = Arc::new(StdMutex::new(None));
        let global_callback_v2 = Arc::new(StdMutex::new(None));
        let rust_event_callback = Arc::new(StdMutex::new(None));
        let (event_dispatch_tx, event_dispatch_rx) = mpsc::unbounded_channel();
        spawn_event_dispatch_thread(
            global_callback.clone(),
            global_callback_v2.clone(),
            rust_event_callback.clone(),
            event_dispatch_rx,
        );

        Self {
            global_callback,
            global_callback_v2,
            rust_event_callback,
            event_dispatch_tx,
            log_callback: StdMutex::new(None),
            sessions: StdRwLock::new(Vec::new()),
            config: StdMutex::new(Config::new_default()),
            runtime: Runtime::new().expect("Failed to create Tokio runtime"),
            self_id: StdMutex::new(NodeId("local".to_string())),
            l0: Arc::new(crate::layers::native_l0::NativeL0::new()),
            initialized: AtomicBool::new(false),
        }
    }

    pub async fn sessions_snapshot(&self) -> Vec<(String, Arc<SessionCtx>)> {
        self.sessions.read().unwrap().clone()
    }

    pub async fn get_session(&self, room_id: &str) -> Option<Arc<SessionCtx>> {
        self.get_session_sync(room_id)
    }

    /// Synchronous equivalent of [`get_session`](Self::get_session) -- see
    /// `MistEngine::sessions`'s doc comment for why this needs to exist at
    /// all: `handle_action_in_room` (`engine/action.rs`) must resolve a
    /// session without an `.await`, so an `OverlayAction::SendMessage` can be
    /// enqueued inline, in the exact order it was handed to `handle_action`,
    /// instead of via a `tokio::spawn` whose execution order vs. other
    /// spawned lookups is not guaranteed.
    pub(crate) fn get_session_sync(&self, room_id: &str) -> Option<Arc<SessionCtx>> {
        self.sessions
            .read()
            .unwrap()
            .iter()
            .find(|(id, _)| id == room_id)
            .map(|(_, ctx)| ctx.clone())
    }

    pub async fn has_session(&self, room_id: &str) -> bool {
        self.sessions
            .read()
            .unwrap()
            .iter()
            .any(|(id, _)| id == room_id)
    }

    /// The first-joined session: the implicit target for roomless FFI
    /// operations that have no room of their own to scope to (SPEC-15 rule 5).
    pub async fn primary_session(&self) -> Option<Arc<SessionCtx>> {
        self.sessions
            .read()
            .unwrap()
            .first()
            .map(|(_, ctx)| ctx.clone())
    }

    /// Inserts a newly-built session. Returns `false` (without inserting) if
    /// a session for this room is already active -- either a legitimate
    /// re-join (see `room::join_room`) or a race with a concurrent
    /// `join_room` call for the same room; either way, the caller must not
    /// start (`ENGINE.run`) the session it just built.
    pub async fn insert_session(&self, room_id: String, ctx: Arc<SessionCtx>) -> bool {
        let mut sessions = self.sessions.write().unwrap();
        if sessions.iter().any(|(id, _)| *id == room_id) {
            return false;
        }
        sessions.push((room_id, ctx));
        true
    }

    pub async fn remove_session(&self, room_id: &str) -> Option<Arc<SessionCtx>> {
        let mut sessions = self.sessions.write().unwrap();
        let index = sessions.iter().position(|(id, _)| id == room_id)?;
        Some(sessions.remove(index).1)
    }

    pub async fn remove_all_sessions(&self) -> Vec<(String, Arc<SessionCtx>)> {
        let mut sessions = self.sessions.write().unwrap();
        std::mem::take(&mut *sessions)
    }

    /// Picks which active session a roomless unicast send to `target` should
    /// go through: the first (in join order) whose node store already knows
    /// about `target`, falling back to the first joined session (SPEC-15
    /// rule 5).
    pub async fn resolve_unicast_session(&self, target: &NodeId) -> Option<Arc<SessionCtx>> {
        let sessions = self.sessions_snapshot().await;
        for (_, ctx) in &sessions {
            if ctx.node_store.lock().unwrap().nodes.contains_key(target) {
                return Some(ctx.clone());
            }
        }
        sessions.into_iter().next().map(|(_, ctx)| ctx)
    }
}

fn spawn_event_dispatch_thread(
    global_callback: Arc<StdMutex<Option<EventCallback>>>,
    global_callback_v2: Arc<StdMutex<Option<EventCallbackV2>>>,
    rust_event_callback: Arc<StdMutex<Option<RustEventCallback>>>,
    mut rx: mpsc::UnboundedReceiver<(u32, String, NodeId, Vec<u8>)>,
) {
    std::thread::spawn(move || {
        while let Some((message_type, room_id, from, data)) = rx.blocking_recv() {
            if let Some(cb) = rust_event_callback.lock().unwrap().clone() {
                cb(message_type, from.0.clone(), data.clone());
            }

            let v1 = *global_callback.lock().unwrap();
            let v2 = *global_callback_v2.lock().unwrap();

            if v1.is_none() && v2.is_none() {
                if matches!(message_type, EVENT_JOIN | EVENT_LEAVE) {
                    tracing::debug!(
                        "dropping FFI join/leave event for {from}: no callback registered"
                    );
                }
                mistlib_core::stats::STATS.add_dropped_ffi_event();
                continue;
            }

            if let Some(cb) = v1 {
                // SAFETY: cb is a valid function pointer registered by the caller; from and data
                // are owned by this dispatch thread and remain alive for the duration of the call.
                unsafe {
                    cb(
                        message_type,
                        from.0.as_ptr(),
                        from.0.len(),
                        data.as_ptr(),
                        data.len(),
                    );
                }
            }
            if let Some(cb) = v2 {
                // SAFETY: same as above, plus room_id is owned by this dispatch thread and
                // remains alive for the duration of the call.
                unsafe {
                    cb(
                        message_type,
                        room_id.as_ptr(),
                        room_id.len(),
                        from.0.as_ptr(),
                        from.0.len(),
                        data.as_ptr(),
                        data.len(),
                    );
                }
            }
        }
    });
}

pub(crate) fn rust_node_id() -> NodeId {
    NodeId("rust".to_string())
}

pub static ENGINE: std::sync::LazyLock<MistEngine> = std::sync::LazyLock::new(MistEngine::new);

#[cfg(test)]
pub(crate) mod tests;
