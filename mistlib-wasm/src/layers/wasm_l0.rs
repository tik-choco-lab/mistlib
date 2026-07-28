// MULTI-ROOM CONTRACT v1
//
// A "session" is a full per-room stack: its own signaler connection, its own
// `WasmWebRtcTransport`, and its own core `MistEngine` (overlay/DNVE3),
// keyed by room_id in `crate::app`'s session registry. All sessions share
// one self NodeId and one Config (`crate::app::self_id`/`base_config`).
//
// `init`/`init_with_config` (via `L0Engine::initialize`) only record the
// self id and config -- they must not build a session. `join_room` is what
// builds one (`build_session`), the first time a given room_id is joined;
// joining an already-active room is just an idempotent re-announce
// (`request_peers`), matching the pre-multi-room behavior for a single
// room. `leave_room`/`leave_room_id` tear a session down completely
// (`teardown_session`) rather than reusing it, so a subsequent join to the
// same room always builds a brand new engine/transport/signaler -- this
// sidesteps the SPEC-11 leave/rejoin race (a stale transport's in-flight
// disconnects can't collide with a new session's connects, because there is
// no shared transport instance to race on).
use crate::app::{Session, WasmEngineEventHandler};
use crate::layers::wasm_l1::WasmL1Transport;
use crate::signaling::{WasmBootstrapSignaler, WasmNostrSignaler, WasmWebSocketSignaler};
use crate::transport::webrtc::WasmWebRtcTransport;
use async_trait::async_trait;
use mistlib_core::config::{Config, SignalingMode};
use mistlib_core::engine::{MistEngine, RunningContext};
use mistlib_core::layers::L0Engine;
use mistlib_core::overlay::dnve3::strategy::DNVE3Strategy;
use mistlib_core::overlay::OverlayRouter;
use mistlib_core::signaling::{
    MessageContent, RoutedSignaler, RoutedSignalingHandler, Signaler, SignalingHandler,
    SignalingRoute,
};
use mistlib_core::stats::EngineStats;
use mistlib_core::types::NodeId;
use std::sync::Arc;
use tokio::sync::mpsc;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;

pub struct WasmL0;

impl WasmL0 {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WasmL0 {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl L0Engine for WasmL0 {
    fn initialize(&self, local_id: NodeId, signaling_url: String) {
        crate::app::set_self_id(local_id);
        crate::app::update_base_config(|config| config.signaling_url = signaling_url);

        // Storage is process-wide (rule 10) and doesn't need a session to
        // exist yet, so it's set up here rather than in `build_session`.
        let storage_config = crate::app::base_config().storage;
        crate::storage::init_storage(&storage_config);
    }

    fn join_room(&self, room_id: String) {
        // `reserve_join` runs synchronously, before this function returns --
        // see its doc comment for why that matters (a same-JS-tick
        // `leave_room_id`/`leave_room` right after this call must observe
        // its effects, not race a still-unscheduled microtask). Only the
        // network-bound tail (`run_join`) is deferred; callers that need to
        // know when the room is actually usable (or that the join failed)
        // should listen for EVENT_ROOM_JOINED/EVENT_ROOM_JOIN_FAILED or use
        // `join_room_async` instead.
        let reservation = reserve_join(&room_id);
        spawn_local(async move {
            let _ = run_join(room_id, reservation).await;
        });
    }

    fn leave_room(&self) {
        crate::app::cancel_all_pending_joins();
        for (room_id, session) in crate::app::drain_all_sessions() {
            teardown_session(&session);
            crate::app::emit_room_left(room_id);
        }
    }

    fn set_config(&self, config: Config) {
        crate::app::set_base_config(config.clone());
        crate::app::for_each_session_engine(|engine| {
            *engine.config.lock().unwrap() = config.clone();
        });
    }

    fn get_config(&self) -> Config {
        crate::app::base_config()
    }

    async fn get_stats(&self) -> EngineStats {
        let stats_str = crate::app::get_stats();
        serde_json::from_str(&stats_str).unwrap_or_else(|_| EngineStats {
            message_count: 0,
            send_bits: 0,
            receive_bits: 0,
            rtt_millis: std::collections::HashMap::new(),
            memory_mb: 0.0,
            world_send_bits: 0,
            world_receive_bits: 0,
            world_message_count: 0,
            relay_send_bits: 0,
            relay_receive_bits: 0,
            relay_message_count: 0,
            dropped_receive_events: 0,
            dropped_ffi_events: 0,
            nodes: vec![],
            diag_peers: 0,
            diag_connection_states: 0,
            diag_pending_candidates: 0,
        })
    }

    async fn storage_add(&self, name: &str, data: &[u8]) -> mistlib_core::error::Result<String> {
        crate::storage::storage_add(name.to_string(), data)
            .await
            .map_err(|e| {
                mistlib_core::error::MistError::Internal(
                    e.as_string()
                        .unwrap_or_else(|| "Unknown storage error".to_string()),
                )
            })
    }

    async fn storage_get(&self, cid: &str) -> mistlib_core::error::Result<Vec<u8>> {
        crate::storage::storage_get(cid.to_string())
            .await
            .map_err(|e| {
                mistlib_core::error::MistError::Internal(
                    e.as_string()
                        .unwrap_or_else(|| "Unknown storage error".to_string()),
                )
            })
            .map(|arr| arr.to_vec())
    }
}

/// The outcome of a join's synchronous reservation step (`reserve_join`),
/// carrying whatever state its async tail (`run_join`) needs to finish the
/// job. See `reserve_join`'s doc comment for why this split exists.
pub(crate) enum JoinReservation {
    /// The room already had an active session at reservation time.
    /// `EVENT_ROOM_JOINED` has already been emitted (see `reserve_join`);
    /// `run_join` only has the re-announce left to do. The webrtc handle is
    /// captured here rather than re-looked-up later, so a leave that lands
    /// between reservation and `run_join` running can't make it operate on
    /// a since-torn-down session -- re-announcing on an already-torn-down
    /// transport is harmless, it just no-ops. `None` if the session
    /// vanished between the `session_exists` check and the webrtc lookup
    /// (shouldn't happen in practice; defensive).
    AlreadyActive(Option<Arc<WasmWebRtcTransport>>),
    /// A build for this room was already in flight; this call is piggy-
    /// backing on it instead of starting a duplicate one.
    PiggyBack(tokio::sync::oneshot::Receiver<Result<(), String>>),
    /// This call owns a brand new build.
    Owner,
}

/// Synchronous reservation step of a join -- the half of the old
/// (pre-fix) `join_room_inner` that must NOT be deferred into a spawned
/// future or async fn body, because doing so breaks the SPEC-15
/// leave-during-pending-join last-write-wins rule: a `leave_room_id`/
/// `leave_room` arriving in the same JS tick right after a `join_room`/
/// `join_room_async` call needs to observe THIS call's effect on
/// `PENDING_JOINS`/`CANCELLED_PENDING` immediately, not race a
/// still-unscheduled microtask (which is what a bare
/// `spawn_local(async move { session_exists(..); mark_join_pending(..); .. })`
/// -- or an `async fn` whose body isn't polled until later -- would do).
/// Contains no `.await` anywhere, by construction.
///
/// Callers: `L0Engine::join_room` calls this directly before returning, then
/// spawns `run_join` with the result. `join_room_async` (in `crate::app`)
/// calls this directly before constructing the `Promise` it returns, for
/// the same reason.
pub(crate) fn reserve_join(room_id: &str) -> JoinReservation {
    if crate::app::session_exists(room_id) {
        // Already active: idempotent re-announce, same as the single-room
        // behavior this replaces (room switching without leaving keeps its
        // request_peers()-only semantics; SPEC-11 scopes rejoin-after-leave
        // out of this path).
        //
        // EVENT_ROOM_JOINED fires HERE, synchronously with this
        // `session_exists` observation, rather than after `run_join`'s
        // `request_peers().await` the way the pre-fix code did it: emitting
        // after an await left a window where an intervening leave could
        // land first, producing a LEFT-then-JOINED inversion for listeners.
        // Emitting now, while the session is provably still active, can't
        // race that. (See its doc comment for why re-join must still
        // signal ready at all.)
        crate::app::emit_room_joined(room_id.to_string());
        return JoinReservation::AlreadyActive(crate::app::session_webrtc(room_id));
    }

    if !crate::app::mark_join_pending(room_id) {
        // A build for this room is already in flight (building spans several
        // .await points before the session is inserted). If an intervening
        // leave_room_id() cancelled it, this join un-cancels it -- a join ->
        // leave -> join sequence nets to joined. Either way this call itself
        // doesn't own the build; it registers a waiter and `run_join` will
        // wait for whichever call does own it to finish, relaying its
        // outcome instead of starting (or emitting for) a duplicate build.
        crate::app::clear_cancelled_pending(room_id);
        return JoinReservation::PiggyBack(crate::app::register_join_waiter(room_id));
    }

    JoinReservation::Owner
}

/// Async tail of a join, run against the `JoinReservation` that
/// `reserve_join` already made synchronously. Shared by the fire-and-forget
/// `L0Engine::join_room` and the awaitable `join_room_async` FFI export.
///
/// Emits exactly one of `EVENT_ROOM_JOINED` / `EVENT_ROOM_JOIN_FAILED` per
/// call that isn't just piggy-backing on someone else's in-flight build (the
/// `AlreadyActive` case already emitted `EVENT_ROOM_JOINED` synchronously in
/// `reserve_join`, before this function ever runs), and drains every waiter
/// registered against this room_id on every exit path of the `Owner` case --
/// a waiter left undrained hangs its caller's `.await` forever.
pub(crate) async fn run_join(room_id: String, reservation: JoinReservation) -> Result<(), JsValue> {
    match reservation {
        JoinReservation::AlreadyActive(webrtc) => {
            if let Some(webrtc) = webrtc {
                let _ = webrtc.request_peers().await;
            }
            Ok(())
        }
        JoinReservation::PiggyBack(waiter) => match waiter.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => Err(JsValue::from_str(&reason)),
            Err(_) => Err(JsValue::from_str(&format!(
                "join_room({}) waiter was dropped without a result",
                room_id
            ))),
        },
        JoinReservation::Owner => {
            let local_id = crate::app::self_id();
            let config = crate::app::base_config();
            let result = build_session(room_id.clone(), local_id, config).await;
            crate::app::clear_join_pending(&room_id);
            match result {
                Ok(session) => {
                    if crate::app::clear_cancelled_pending(&room_id) {
                        // An explicit leave_room_id() arrived while this
                        // session was still building; honor it now rather
                        // than resurrecting a room the caller already asked
                        // to leave.
                        tracing::info!(
                            "WASM join_room({}) was cancelled by an intervening leave; discarding the freshly built session",
                            room_id
                        );
                        teardown_session(&session);
                        let reason = "join cancelled by leave".to_string();
                        crate::app::drain_join_waiters(&room_id, Err(reason.clone()));
                        crate::app::emit_room_join_failed(room_id, reason.clone());
                        Err(JsValue::from_str(&reason))
                    } else {
                        crate::app::insert_session(room_id.clone(), session);
                        crate::app::drain_join_waiters(&room_id, Ok(()));
                        crate::app::emit_room_joined(room_id);
                        Ok(())
                    }
                }
                Err(err) => {
                    // Consume any cancellation marker set while this build was
                    // in flight: leaving it behind would make the NEXT
                    // (unrelated) join_room for this room see a stale cancel
                    // and silently discard its freshly built session.
                    crate::app::clear_cancelled_pending(&room_id);
                    tracing::error!("WASM join_room({}) failed: {:?}", room_id, err);
                    let reason = format!("{err:?}");
                    crate::app::drain_join_waiters(&room_id, Err(reason.clone()));
                    crate::app::emit_room_join_failed(room_id, reason);
                    Err(err)
                }
            }
        }
    }
}

/// New export backing `leave_room_id`: tears down exactly one room's
/// session, leaving every other joined room untouched. Not part of
/// `L0Engine` (its signature is fixed by mistlib-core and shared with
/// mistlib-native), so it's a free function called directly from
/// `crate::app::leave_room_id`.
pub(crate) fn leave_room_id(room_id: &str) -> Result<(), JsValue> {
    if let Some(session) = crate::app::remove_session(room_id) {
        teardown_session(&session);
        crate::app::emit_room_left(room_id.to_string());
        return Ok(());
    }

    if crate::app::cancel_pending_join(room_id) {
        // No session exists yet, but a join_room(room_id) build is still in
        // flight; mark it cancelled so build completion tears it down
        // instead of inserting it. The leave is accepted immediately rather
        // than erroring, even though the session it's cancelling doesn't
        // exist yet.
        tracing::debug!(
            "WASM leave_room_id({}) cancelled an in-flight join",
            room_id
        );
        return Ok(());
    }

    Err(JsValue::from_str(&format!("Room not joined: {}", room_id)))
}

fn teardown_session(session: &Session) {
    session.webrtc.close_all_peer_connections();
    // Schedules the async disconnects/websocket close and returns state to
    // Idle; the spawned cleanup task holds its own clones of what it needs,
    // so it keeps running after `session` (and this engine handle) is
    // dropped.
    session.engine.leave_room();
}

/// Builds and starts a brand new per-room session: its own signaler
/// connection, transport, and `MistEngine`. Mirrors what the pre-multi-room
/// `WasmL0::initialize` used to do inline, parameterized over `room_id` so
/// it can run once per joined room instead of once per process.
async fn build_session(
    room_id: String,
    local_id: NodeId,
    config: Config,
) -> Result<Session, JsValue> {
    let engine = MistEngine::new(Arc::new(crate::runtime::WasmRuntime));
    *engine.self_id.lock().unwrap() = local_id.clone();
    *engine.config.lock().unwrap() = config.clone();
    engine.set_event_handler(Arc::new(WasmEngineEventHandler::new(room_id.clone())));

    let config = Arc::new(config);
    let signaler = build_bootstrap_signaler(&config, &local_id, &config.signaling_url);

    let mut router = OverlayRouter::new(&config, engine.node_store.clone(), local_id.clone());
    router.add_strategy(Arc::new(DNVE3Strategy::new(
        &config,
        engine.node_store.clone(),
        local_id.clone(),
        router.routing_table.clone(),
    )));
    let router_arc = Arc::new(router);

    struct EngineActionHandler(Arc<MistEngine>);
    impl mistlib_core::overlay::ActionHandler for EngineActionHandler {
        fn handle_action(&self, action: mistlib_core::action::OverlayAction) {
            self.0.handle_action(action);
        }
    }
    let action_handler = Arc::new(EngineActionHandler(engine.clone()));

    router_arc
        .start(
            Arc::new(crate::runtime::WasmRuntime),
            config.clone(),
            action_handler.clone(),
        )
        .await;

    let overlay_transport = Arc::new(mistlib_core::overlay::OverlayTransport {
        router: router_arc.clone(),
        action_handler: action_handler.clone(),
    });

    let routed_signaler = Arc::new(RoutedSignaler::new(
        signaler.clone() as Arc<dyn Signaler>,
        overlay_transport.clone(),
    ));
    let webrtc = Arc::new(WasmWebRtcTransport::new(
        routed_signaler.clone() as Arc<dyn Signaler>,
        local_id.clone(),
    ));
    webrtc.set_room_id(room_id.clone());
    webrtc.set_max_connections(config.limits.max_connection_count);
    webrtc.set_max_message_bytes(config.limits.max_message_bytes);
    webrtc.set_ice_servers(config.webrtc.ice_servers.clone());
    let ws_signaling_handler = Arc::new(RoutedSignalingHandler::new(
        routed_signaler.clone(),
        webrtc.clone() as Arc<dyn SignalingHandler>,
        SignalingRoute::WebSocket,
    ));
    let p2p_signaling_handler = Arc::new(RoutedSignalingHandler::new(
        routed_signaler.clone(),
        webrtc.clone() as Arc<dyn SignalingHandler>,
        SignalingRoute::Overlay,
    ));

    let l1 = Arc::new(WasmL1Transport::new(
        overlay_transport.clone() as Arc<dyn mistlib_core::transport::Transport>,
        engine.node_store.clone(),
        local_id.clone(),
    ));

    let ctx = RunningContext {
        transport: overlay_transport.clone(),
        network_transport: Some(webrtc.clone() as Arc<dyn mistlib_core::transport::Transport>),
        signaling_handler: ws_signaling_handler,
        p2p_signaling_handler: Some(p2p_signaling_handler),
        signaling_dispatch: Some(overlay_transport.clone() as Arc<dyn Signaler>),
        websocket_signaler: Some(signaler.clone() as Arc<dyn Signaler>),
        overlay: Some(router_arc),
    };

    let webrtc_for_reconnect = webrtc.clone();
    signaler.set_on_session_reestablished(Arc::new(move || {
        let webrtc = webrtc_for_reconnect.clone();
        spawn_local(async move {
            if let Err(err) = webrtc.request_peers().await {
                tracing::warn!(
                    "WASM signaling peer request failed after reconnect: {:?}",
                    err
                );
            }
        });
    }));

    let (sig_tx, sig_rx) = mpsc::unbounded_channel::<MessageContent>();
    if let Err(err) = signaler.connect(sig_tx).await {
        tracing::error!("WASM signaling connection failed: {:?}", err);
        return Err(err);
    }

    let engine_for_run = engine.clone();
    spawn_local(async move {
        let _ = engine_for_run.run(ctx, sig_rx).await;
    });

    Ok(Session {
        engine,
        webrtc,
        l1_transport: l1,
    })
}

fn build_bootstrap_signaler(
    config: &Config,
    local_id: &NodeId,
    signaling_url: &str,
) -> Arc<WasmBootstrapSignaler> {
    match config.signaling.mode {
        SignalingMode::WebSocket => Arc::new(WasmBootstrapSignaler::WebSocket(Arc::new(
            WasmWebSocketSignaler::new(signaling_url),
        ))),
        SignalingMode::Nostr => {
            let nostr_config = config
                .signaling
                .nostr
                .clone()
                .expect("Nostr signaling mode requires validated Nostr config");
            Arc::new(WasmBootstrapSignaler::Nostr(Arc::new(
                WasmNostrSignaler::new(local_id.clone(), nostr_config),
            )))
        }
    }
}
