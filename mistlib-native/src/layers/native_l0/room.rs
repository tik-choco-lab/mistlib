use crate::engine::{SessionCtx, ENGINE};
use crate::events::{dispatch_event, EVENT_ROOM_JOINED, EVENT_ROOM_JOIN_FAILED, EVENT_ROOM_LEFT};
use mistlib_core::signaling::Signaler;
use mistlib_core::types::NodeId;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};

use super::init::build_session_context;

// Reservation happens before returning to the host, including while startup
// awaits signaling. The bool records an intervening leave. A subsequent join
// clears it and piggybacks on the same build, as on wasm.
#[derive(Default)]
struct PendingJoins(HashMap<String, bool>);

impl PendingJoins {
    fn reserve(&mut self, room_id: &str) -> bool {
        if let Some(cancelled) = self.0.get_mut(room_id) {
            *cancelled = false;
            false
        } else {
            self.0.insert(room_id.to_string(), false);
            true
        }
    }

    fn cancel(&mut self, room_id: &str) -> bool {
        if let Some(cancelled) = self.0.get_mut(room_id) {
            *cancelled = true;
            true
        } else {
            false
        }
    }
}

// Lock order: JOINS -> ENGINE.sessions. Never held across an await or teardown.
static JOINS: LazyLock<Mutex<PendingJoins>> = LazyLock::new(|| Mutex::new(PendingJoins::default()));

fn room_event(code: u32, room_id: &str, payload: &[u8]) {
    dispatch_event(code, room_id, &NodeId(String::new()), payload);
}

pub(super) fn join_room(room_id: String) {
    if !ENGINE.initialized.load(Ordering::Relaxed) {
        room_event(
            EVENT_ROOM_JOIN_FAILED,
            &room_id,
            b"init() has not been called yet",
        );
        return;
    }

    {
        let mut joins = JOINS.lock().unwrap();
        if !joins.0.contains_key(&room_id) {
            if let Some(ctx) = ENGINE.get_session_sync(&room_id) {
                // Report readiness before any await so a leave cannot overtake it.
                room_event(EVENT_ROOM_JOINED, &room_id, &[]);
                ENGINE.runtime.spawn(async move { reannounce(&ctx).await });
                return;
            }
        }
        if !joins.reserve(&room_id) {
            return;
        }
    }
    ENGINE.runtime.spawn(run_join(room_id));
}

async fn run_join(room_id: String) {
    let local_id = ENGINE.self_id.lock().unwrap().clone();
    let ctx = build_session_context(room_id.clone(), local_id).await;
    ENGINE.insert_session(room_id.clone(), ctx.clone()).await;
    let result = ENGINE.start_session(&ctx).await;
    complete_join(ctx, result).await;
}

async fn complete_join(
    ctx: Arc<SessionCtx>,
    result: crate::error::Result<
        tokio::sync::mpsc::UnboundedReceiver<mistlib_core::transport::NetworkEvent>,
    >,
) {
    let room_id = ctx.room_id.clone();
    let failure = {
        let mut joins = JOINS.lock().unwrap();
        match result {
            Ok(network_rx) if !joins.0[&room_id] => {
                joins.0.remove(&room_id);
                ENGINE.spawn_background_loops(ctx.clone());
                room_event(EVENT_ROOM_JOINED, &room_id, &[]);
                ENGINE.runtime.spawn(async move {
                    ENGINE.process_network_events(network_rx, ctx).await;
                });
                return;
            }
            Ok(_) => "join cancelled by leave".to_string(),
            Err(err) => err.to_string(),
        }
    };
    // A failed/cancelled startup never became usable: clean it up and report
    // JOIN_FAILED, with no ROOM_LEFT (the same rule as wasm's build path).
    ENGINE.remove_session(&room_id).await;
    teardown(&ctx).await;
    let mut joins = JOINS.lock().unwrap();
    joins.0.remove(&room_id);
    room_event(EVENT_ROOM_JOIN_FAILED, &room_id, failure.as_bytes());
}

async fn reannounce(ctx: &SessionCtx) {
    if let Some(wt) = ctx.webrtc_transport.as_ref() {
        if let Err(err) = wt.announce_to_room().await {
            tracing::warn!(
                "join_room: re-announce failed for {}: {:?}",
                ctx.room_id,
                err
            );
        }
    }
}

pub(super) fn leave_room() {
    let active = {
        let mut joins = JOINS.lock().unwrap();
        for cancelled in joins.0.values_mut() {
            *cancelled = true;
        }
        let mut sessions = ENGINE.sessions.write().unwrap();
        let (pending, active) = std::mem::take(&mut *sessions)
            .into_iter()
            .partition::<Vec<_>, _>(|(room, _)| joins.0.contains_key(room));
        *sessions = pending;
        active
    };
    finish_leave(active);
}

pub(super) fn leave_room_id(room_id: String) {
    let active = {
        let mut joins = JOINS.lock().unwrap();
        if joins.cancel(&room_id) {
            return;
        }
        let mut sessions = ENGINE.sessions.write().unwrap();
        sessions
            .iter()
            .position(|(id, _)| id == &room_id)
            .map(|index| vec![sessions.remove(index)])
            .unwrap_or_default()
    };
    finish_leave(active);
}

fn finish_leave(active: Vec<(String, Arc<SessionCtx>)>) {
    let cleanup = async move {
        for (room_id, ctx) in active {
            teardown(&ctx).await;
            room_event(EVENT_ROOM_LEFT, &room_id, &[]);
        }
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        ENGINE.runtime.spawn(cleanup);
    } else {
        ENGINE.runtime.block_on(cleanup);
    }
}

async fn teardown(ctx: &SessionCtx) {
    // Stops this session's background loops, network event pump, and
    // signaling loop -- and only this session's; every other active room's
    // cancel token is untouched.
    ctx.cancel.cancel();

    if let Some(wt) = ctx.webrtc_transport.as_ref() {
        wt.close_all_peer_connections().await;
        wt.stop_session_sweeper();
    } else {
        crate::storage::WANT_REGISTRY.room_reset(&ctx.room_id);
    }
    if let Some(signaler) = ctx.bootstrap_signaler.as_ref() {
        if let Err(err) = signaler.close().await {
            tracing::warn!(
                "leave_room: bootstrap signaling close failed for {}: {:?}",
                ctx.room_id,
                err
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_join_cancellation_and_rejoin_are_room_scoped() {
        let mut pending = PendingJoins::default();
        assert!(pending.reserve("a"));
        assert!(pending.reserve("b"));
        assert!(pending.cancel("a"));
        assert!(pending.0["a"]);
        assert!(!pending.0["b"]);
        assert!(!pending.reserve("a"));
        assert!(!pending.0["a"]);
        assert!(!pending.cancel("missing"));
    }
    #[test]
    fn room_callbacks_report_readiness_failure_cancellation_and_actual_leave() {
        use crate::engine::tests::session_registry::{fake_session, REGISTRY_TEST_LOCK};
        use crate::events::tests::TEST_LOCK;
        use std::time::Duration;
        let _callbacks = TEST_LOCK.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        *ENGINE.rust_event_callback.lock().unwrap() = Some(Arc::new(move |code, from, data| {
            tx.send((code, from, data)).unwrap();
        }));
        ENGINE.runtime.block_on(async {
            let _registry = REGISTRY_TEST_LOCK.lock().await;
            let room = "event-parity-room";
            let prepare = || async {
                JOINS.lock().unwrap().reserve(room);
                let ctx = fake_session(room);
                assert!(ENGINE.insert_session(room.to_string(), ctx.clone()).await);
                ctx
            };
            let receive_room_event = || loop {
                let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if matches!(
                    event.0,
                    EVENT_ROOM_JOINED | EVENT_ROOM_JOIN_FAILED | EVENT_ROOM_LEFT
                ) {
                    assert!(event.1.is_empty());
                    return (event.0, event.2);
                }
            };
            let ctx = prepare().await;
            complete_join(
                ctx,
                Err(crate::error::MistError::Network("startup failed".into())),
            )
            .await;
            let failure = receive_room_event();
            assert_eq!(failure.0, EVENT_ROOM_JOIN_FAILED);
            assert!(String::from_utf8(failure.1)
                .unwrap()
                .contains("startup failed"));
            assert!(!ENGINE.has_session(room).await);

            let ctx = prepare().await;
            leave_room_id(room.to_string());
            let (_, network_rx) = tokio::sync::mpsc::unbounded_channel();
            complete_join(ctx, Ok(network_rx)).await;
            assert_eq!(
                receive_room_event(),
                (EVENT_ROOM_JOIN_FAILED, b"join cancelled by leave".to_vec())
            );
            assert!(!ENGINE.has_session(room).await);

            let ctx = prepare().await;
            let (_, network_rx) = tokio::sync::mpsc::unbounded_channel();
            complete_join(ctx, Ok(network_rx)).await;
            assert_eq!(receive_room_event(), (EVENT_ROOM_JOINED, vec![]));
            // Re-announcing an active room reports readiness again.
            let was_initialized = ENGINE.initialized.swap(true, Ordering::Relaxed);
            join_room(room.to_string());
            ENGINE.initialized.store(was_initialized, Ordering::Relaxed);
            assert_eq!(receive_room_event(), (EVENT_ROOM_JOINED, vec![]));
            leave_room_id(room.to_string());
            assert_eq!(receive_room_event(), (EVENT_ROOM_LEFT, vec![]));
            assert!(!ENGINE.has_session(room).await);
            leave_room_id(room.to_string());
            // Queue a barrier so missing-room leave cannot hide an extra event.
            room_event(crate::events::EVENT_OVERLAY, room, b"barrier");
            loop {
                let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(!matches!(
                    event.0,
                    EVENT_ROOM_JOINED | EVENT_ROOM_JOIN_FAILED | EVENT_ROOM_LEFT
                ));
                if event.2 == b"barrier" {
                    break;
                }
            }
        });
        *ENGINE.rust_event_callback.lock().unwrap() = None;
    }
}
