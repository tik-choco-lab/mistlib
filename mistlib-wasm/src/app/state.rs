use super::Session;
use crate::session_registry::SessionRegistry;
use crate::transport::webrtc::WasmWebRtcTransport;
use mistlib_core::config::Config;
use mistlib_core::engine::MistEngine;
use mistlib_core::types::NodeId;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

thread_local! {
    // One self NodeId and one Config shared by every session (multi-room
    // contract point 1). `init`/`init_with_config`/`set_config` write here;
    // `join_room` reads it when building a new session's engine.
    static SELF_ID: RefCell<NodeId> = RefCell::new(NodeId("local".to_string()));
    static BASE_CONFIG: RefCell<Config> = RefCell::new(Config::new_default());
    pub(crate) static EVENT_CALLBACK: RefCell<Option<js_sys::Function>> = RefCell::new(None);
    pub(crate) static MEDIA_EVENT_CALLBACK: RefCell<Option<js_sys::Function>> = RefCell::new(None);
    pub(crate) static SESSIONS: RefCell<SessionRegistry<Session>> = RefCell::new(SessionRegistry::new());
    // Rooms whose `build_session` is in flight but not yet inserted into
    // SESSIONS (building involves several `.await`s). Without this, two
    // `join_room(room)` calls for the same not-yet-active room arriving
    // before the first finishes would both see "not active" and race to
    // build duplicate sessions.
    static PENDING_JOINS: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
    // Rooms in PENDING_JOINS whose build has been cancelled by an
    // intervening `leave_room_id`/`leave_room` before it could finish. The
    // in-flight build still runs to completion (there's no way to abort a
    // spawned future from here), but consults this set right before
    // inserting into SESSIONS so the explicit leave isn't silently
    // overwritten by the join it raced with.
    static CANCELLED_PENDING: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
    // Oneshot senders for callers piggy-backing on room_id's in-flight join
    // build (`mark_join_pending` returned false) instead of starting a
    // duplicate build -- see `register_join_waiter`/`drain_join_waiters` and
    // `reserve_join`/`run_join` in `layers::wasm_l0`. Every sender pushed here MUST
    // be drained exactly once, on every exit path of the owning build, or a
    // waiting caller's `.await` never resolves.
    static JOIN_WAITERS: RefCell<JoinWaiters> = RefCell::new(HashMap::new());
}

type JoinWaiters = HashMap<String, Vec<tokio::sync::oneshot::Sender<Result<(), String>>>>;

pub(crate) fn self_id() -> NodeId {
    SELF_ID.with(|id| id.borrow().clone())
}

pub(crate) fn set_self_id(id: NodeId) {
    SELF_ID.with(|cell| *cell.borrow_mut() = id);
}

pub(crate) fn base_config() -> Config {
    BASE_CONFIG.with(|c| c.borrow().clone())
}

pub(crate) fn set_base_config(config: Config) {
    BASE_CONFIG.with(|c| *c.borrow_mut() = config);
}

pub(crate) fn update_base_config(f: impl FnOnce(&mut Config)) {
    BASE_CONFIG.with(|c| f(&mut c.borrow_mut()));
}

pub(crate) fn session_exists(room_id: &str) -> bool {
    SESSIONS.with(|s| s.borrow().contains(room_id))
}

pub(crate) fn session_webrtc(room_id: &str) -> Option<Arc<WasmWebRtcTransport>> {
    SESSIONS.with(|s| s.borrow().get(room_id).map(|sess| sess.webrtc.clone()))
}

pub(crate) fn insert_session(room_id: String, session: Session) {
    SESSIONS.with(|s| {
        s.borrow_mut().insert(room_id, session);
    });
}

pub(crate) fn remove_session(room_id: &str) -> Option<Session> {
    SESSIONS.with(|s| s.borrow_mut().remove(room_id))
}

pub(crate) fn drain_all_sessions() -> Vec<(String, Session)> {
    SESSIONS.with(|s| s.borrow_mut().drain_all())
}

/// Marks `room_id`'s session build as in flight. Returns `false` (and marks
/// nothing) if one is already pending, so the caller can no-op instead of
/// starting a duplicate `build_session`.
pub(crate) fn mark_join_pending(room_id: &str) -> bool {
    PENDING_JOINS.with(|p| p.borrow_mut().insert(room_id.to_string()))
}

pub(crate) fn clear_join_pending(room_id: &str) {
    PENDING_JOINS.with(|p| {
        p.borrow_mut().remove(room_id);
    });
}

/// Marks `room_id`'s in-flight join (if any) as cancelled, so its build
/// discards the freshly built session instead of inserting it. Returns
/// whether a pending join was actually found (and thus cancelled) --
/// `leave_room_id` uses this to distinguish "cancelled a pending join" from
/// "room isn't joined and nothing is pending" (still an error).
pub(crate) fn cancel_pending_join(room_id: &str) -> bool {
    let is_pending = PENDING_JOINS.with(|p| p.borrow().contains(room_id));
    if is_pending {
        CANCELLED_PENDING.with(|c| {
            c.borrow_mut().insert(room_id.to_string());
        });
    }
    is_pending
}

/// Cancels every currently in-flight join, for `leave_room()` (which tears
/// down every joined room, not just one).
pub(crate) fn cancel_all_pending_joins() {
    let pending: Vec<String> = PENDING_JOINS.with(|p| p.borrow().iter().cloned().collect());
    CANCELLED_PENDING.with(|c| {
        let mut cancelled = c.borrow_mut();
        for room in pending {
            cancelled.insert(room);
        }
    });
}

/// Clears `room_id`'s cancellation mark, returning whether one was set.
/// Used both to un-cancel a rapid join -> leave -> join sequence (return
/// value ignored) and, at build completion, to decide whether the freshly
/// built session should be inserted or torn down (return value checked).
pub(crate) fn clear_cancelled_pending(room_id: &str) -> bool {
    CANCELLED_PENDING.with(|c| c.borrow_mut().remove(room_id))
}

/// Registers a waiter for `room_id`'s in-flight join build, returning a
/// receiver that resolves once the owning `run_join` call drains it
/// (see `drain_join_waiters`). Used by a join call that finds a build
/// already in flight (`mark_join_pending` returned `false`) instead of
/// starting a duplicate build.
pub(crate) fn register_join_waiter(
    room_id: &str,
) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    JOIN_WAITERS.with(|w| {
        w.borrow_mut()
            .entry(room_id.to_string())
            .or_default()
            .push(tx);
    });
    rx
}

/// Notifies every waiter registered for `room_id` (via `register_join_waiter`)
/// with `result`, then forgets them. Must be called exactly once per
/// `room_id` on every exit path of the owning build (success, cancelled by
/// an intervening leave, and build error) -- see `run_join` in
/// `layers::wasm_l0`.
pub(crate) fn drain_join_waiters(room_id: &str, result: Result<(), String>) {
    let waiters = JOIN_WAITERS.with(|w| w.borrow_mut().remove(room_id));
    for tx in waiters.into_iter().flatten() {
        let _ = tx.send(result.clone());
    }
}

/// Applies `f` to every active session's engine, e.g. to propagate a
/// `set_config()` update (config is process-wide, but each engine keeps its
/// own copy -- see the multi-room contract).
pub(crate) fn for_each_session_engine(mut f: impl FnMut(&Arc<MistEngine>)) {
    SESSIONS.with(|s| {
        for (_, sess) in s.borrow().iter_in_join_order() {
            f(&sess.engine);
        }
    });
}
