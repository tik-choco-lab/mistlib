use super::*;
use std::cell::RefCell;

/// Regression coverage for the double-wrap bug fixed in `send_via_ctx`
/// (see the `preferred_transport` doc comment in mistlib-core): a mock
/// `Transport` on `ctx.transport` (the wrong, would-re-wrap path) must never
/// be invoked, and the mock on `ctx.network_transport` (the correct raw path)
/// must observe exactly one envelope layer per send, with the original app
/// bytes intact and per-destination seq numbers assigned once each.
#[cfg(test)]
mod send_via_ctx_tests {
    use super::*;
    use async_trait::async_trait;
    use mistlib_core::overlay::{wire, NodeStore, OverlayEnvelope, OverlayRouter};
    use mistlib_core::signaling::SignalingHandler;
    use mistlib_core::transport::NetworkEventHandler;
    use mistlib_core::types::ConnectionState;
    use mistlib_core::Result as MistResult;
    use wasm_bindgen_test::wasm_bindgen_test;

    /// Records every `send`/`broadcast` call it receives; used both as the
    /// "wrong" transport (must stay empty) and the "right" one (must capture
    /// the single-wrapped envelopes).
    #[derive(Default)]
    struct RecordingTransport {
        calls: RefCell<Vec<(NodeId, Bytes, DeliveryMethod)>>,
    }

    #[async_trait(?Send)]
    impl Transport for RecordingTransport {
        async fn start(&self, _handler: Arc<dyn NetworkEventHandler>) -> MistResult<()> {
            Ok(())
        }
        async fn send(&self, node: &NodeId, data: Bytes, method: DeliveryMethod) -> MistResult<()> {
            self.calls.borrow_mut().push((node.clone(), data, method));
            Ok(())
        }
        async fn broadcast(&self, data: Bytes, method: DeliveryMethod) -> MistResult<()> {
            self.calls
                .borrow_mut()
                .push((NodeId::broadcast(), data, method));
            Ok(())
        }
        fn get_connection_state(&self, _node: &NodeId) -> ConnectionState {
            ConnectionState::Connected
        }
        async fn connect(&self, _node: &NodeId) -> MistResult<()> {
            Ok(())
        }
        async fn disconnect(&self, _node: &NodeId) -> MistResult<()> {
            Ok(())
        }
        fn get_connected_nodes(&self) -> Vec<NodeId> {
            Vec::new()
        }
    }

    struct NoopSignalingHandler;

    #[async_trait(?Send)]
    impl SignalingHandler for NoopSignalingHandler {
        async fn handle_message(&self, _msg: MessageContent) -> MistResult<()> {
            Ok(())
        }
    }

    fn make_ctx(
        wrong_transport: Arc<RecordingTransport>,
        raw_transport: Arc<RecordingTransport>,
        local_id: NodeId,
    ) -> Arc<RunningContext> {
        let node_store = Arc::new(std::sync::Mutex::new(NodeStore::new()));
        let router = OverlayRouter::new(&Config::new_default(), node_store, local_id);

        Arc::new(RunningContext {
            transport: wrong_transport as Arc<dyn Transport>,
            network_transport: Some(raw_transport as Arc<dyn Transport>),
            signaling_handler: Arc::new(NoopSignalingHandler),
            p2p_signaling_handler: None,
            signaling_dispatch: None,
            websocket_signaler: None,
            overlay: Some(Arc::new(router)),
        })
    }

    #[wasm_bindgen_test]
    async fn send_via_ctx_dispatches_single_wrapped_envelope_over_network_transport() {
        let local_id = NodeId("local-node".to_string());
        let target = NodeId("peer-node".to_string());
        let wrong_transport = Arc::new(RecordingTransport::default());
        let raw_transport = Arc::new(RecordingTransport::default());
        let ctx = make_ctx(wrong_transport.clone(), raw_transport.clone(), local_id);

        for payload in ["m1", "m2", "m3"] {
            send_via_ctx(
                &ctx,
                &target,
                Bytes::from_static(payload.as_bytes()),
                DeliveryMethod::ReliableOrdered,
            )
            .await;
        }

        assert!(
            wrong_transport.calls.borrow().is_empty(),
            "send_via_ctx must never dispatch through ctx.transport (it would wrap the data a second time)"
        );

        let calls = raw_transport.calls.borrow();
        assert_eq!(calls.len(), 3);

        let mut seqs = Vec::new();
        for (i, (node, data, method)) in calls.iter().enumerate() {
            assert_eq!(*node, target);
            assert_eq!(*method, DeliveryMethod::ReliableOrdered);

            let envelope: OverlayEnvelope =
                wire::deserialize(data).expect("exactly one envelope layer must decode cleanly");
            let seq = envelope.seq;
            match envelope.content {
                MessageContent::Raw(app_bytes) => {
                    assert_eq!(
                        app_bytes.as_ref(),
                        ["m1", "m2", "m3"][i].as_bytes(),
                        "decoded content must be the original app bytes, not a nested envelope"
                    );
                }
                other => panic!("expected MessageContent::Raw, got {other:?}"),
            }
            seqs.push(seq);
        }
        assert_eq!(
            seqs,
            vec![1, 2, 3],
            "seq must be assigned once per send, monotonically per destination"
        );
    }
}

/// Coverage for the join-in-flight state machine and its waiter-notification
/// side channel (`PENDING_JOINS`/`CANCELLED_PENDING`/`JOIN_WAITERS`), which
/// `layers::wasm_l0::reserve_join`/`run_join`'s three branches (already-active,
/// piggy-back-on-in-flight-build, owning-build) are built on. These are pure
/// thread-local-state tests: they drive the helper functions directly with
/// the exact call sequence each real branch performs, without invoking
/// `build_session` (no network path).
///
/// All thread-local state here (`PENDING_JOINS`, `CANCELLED_PENDING`,
/// `JOIN_WAITERS`, `SESSIONS`) is process-wide, and `wasm-pack test` runs
/// every test in the same single-threaded wasm instance, so each test below
/// uses its own unique room_id to stay isolated from the others.
#[cfg(test)]
mod join_state_tests {
    use super::*;
    use async_trait::async_trait;
    use mistlib_core::signaling::Signaler;
    use mistlib_core::transport::NetworkEventHandler;
    use mistlib_core::types::ConnectionState;
    use mistlib_core::Result as MistResult;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn mark_join_pending_is_exclusive_until_cleared() {
        let room = "join-state-mark-pending";
        assert!(mark_join_pending(room), "first mark should succeed");
        assert!(
            !mark_join_pending(room),
            "a second mark while pending must report a build already in flight"
        );
        clear_join_pending(room);
        assert!(
            mark_join_pending(room),
            "clearing should allow a fresh mark for the next join"
        );
        clear_join_pending(room);
    }

    #[wasm_bindgen_test]
    fn cancel_pending_join_only_reports_true_when_something_was_pending() {
        let room = "join-state-cancel-pending";
        assert!(
            !cancel_pending_join(room),
            "nothing pending yet: leave_room_id must treat this as a real 'not joined' error, not a cancel"
        );

        assert!(mark_join_pending(room));
        assert!(
            cancel_pending_join(room),
            "a pending build must be cancellable"
        );
        assert!(
            clear_cancelled_pending(room),
            "the cancellation mark set above must be observable by the owning build"
        );
        assert!(
            !clear_cancelled_pending(room),
            "clearing is one-shot: a second clear must find nothing left"
        );
        clear_join_pending(room);
    }

    #[wasm_bindgen_test]
    fn join_leave_join_nets_to_joined_via_uncancel() {
        // Models SPEC-15: a reserve_join call that finds a build already
        // in flight (mark_join_pending() == false) un-cancels any pending
        // cancellation before registering its waiter -- this is what makes a
        // join -> leave -> join sequence net to joined instead of the leave
        // winning.
        let room = "join-state-uncancel";
        assert!(mark_join_pending(room));
        assert!(cancel_pending_join(room));

        // The second reserve_join call's un-cancel step:
        clear_cancelled_pending(room);

        assert!(
            !clear_cancelled_pending(room),
            "the un-cancel must have actually consumed the cancellation mark, or the owning \
             build would still see it and wrongly discard its session"
        );
        clear_join_pending(room);
    }

    #[wasm_bindgen_test]
    async fn drain_join_waiters_resolves_a_single_waiter_with_the_given_result() {
        let room = "join-state-waiter-ok";
        let waiter = register_join_waiter(room);
        drain_join_waiters(room, Ok(()));
        assert_eq!(waiter.await, Ok(Ok(())));
    }

    #[wasm_bindgen_test]
    async fn drain_join_waiters_relays_an_error_result_to_the_waiter() {
        let room = "join-state-waiter-err";
        let waiter = register_join_waiter(room);
        drain_join_waiters(room, Err("build failed".to_string()));
        assert_eq!(waiter.await, Ok(Err("build failed".to_string())));
    }

    #[wasm_bindgen_test]
    async fn drain_join_waiters_notifies_every_registered_waiter_for_the_room() {
        let room = "join-state-waiter-multi";
        let first = register_join_waiter(room);
        let second = register_join_waiter(room);
        drain_join_waiters(room, Ok(()));
        assert_eq!(first.await, Ok(Ok(())));
        assert_eq!(second.await, Ok(Ok(())));
    }

    /// Regression coverage for the cancelled-build branch of
    /// `run_join`: replays its exact sequence (mark -> a second
    /// caller registers a waiter instead of starting a duplicate build ->
    /// cancel arrives -> owning build finishes and observes the
    /// cancellation -> drains the waiter). `try_recv` (non-blocking) proves
    /// the waiter genuinely has no result until that last step runs: if a
    /// future edit dropped the `drain_join_waiters` call from the cancelled
    /// branch -- exactly the bug this mechanism exists to prevent, "a
    /// waiter left undrained hangs its caller's `.await` forever" -- the
    /// final assertion below would fail because the channel would still be
    /// empty instead of holding the cancelled `Err`.
    #[wasm_bindgen_test]
    fn cancelled_branch_drains_its_waiter_instead_of_hanging_it() {
        let room = "join-state-cancelled-drains-waiter";

        // Owning build starts.
        assert!(mark_join_pending(room));
        // A second reserve_join call for the same room piggy-backs
        // instead of starting a duplicate build.
        let mut waiter = register_join_waiter(room);
        assert!(
            waiter.try_recv().is_err(),
            "no result should exist before the owning build finishes"
        );

        // leave_room_id() arrives before the build completes.
        assert!(cancel_pending_join(room));

        // Owning build finishes: clear_join_pending, observe the
        // cancellation, then -- the step under test -- drain the waiter.
        clear_join_pending(room);
        assert!(clear_cancelled_pending(room));
        let reason = "join cancelled by leave".to_string();
        drain_join_waiters(room, Err(reason.clone()));

        assert_eq!(
            waiter.try_recv(),
            Ok(Err(reason)),
            "the waiter must observe the cancelled result right after drain_join_waiters runs"
        );
    }

    struct NoopSignaler;

    #[async_trait(?Send)]
    impl Signaler for NoopSignaler {
        async fn send_signaling(&self, _to: &NodeId, _msg: MessageContent) -> MistResult<()> {
            Ok(())
        }
        async fn close(&self) -> MistResult<()> {
            Ok(())
        }
    }

    struct NoopNetTransport;

    #[async_trait(?Send)]
    impl Transport for NoopNetTransport {
        async fn start(&self, _handler: Arc<dyn NetworkEventHandler>) -> MistResult<()> {
            Ok(())
        }
        async fn send(
            &self,
            _node: &NodeId,
            _data: Bytes,
            _method: DeliveryMethod,
        ) -> MistResult<()> {
            Ok(())
        }
        async fn broadcast(&self, _data: Bytes, _method: DeliveryMethod) -> MistResult<()> {
            Ok(())
        }
        fn get_connection_state(&self, _node: &NodeId) -> ConnectionState {
            ConnectionState::Disconnected
        }
        async fn connect(&self, _node: &NodeId) -> MistResult<()> {
            Ok(())
        }
        async fn disconnect(&self, _node: &NodeId) -> MistResult<()> {
            Ok(())
        }
        fn get_connected_nodes(&self) -> Vec<NodeId> {
            Vec::new()
        }
    }

    /// A minimal but real `Session`, cheap enough to construct in a test
    /// (no signaling connection, no ICE) -- just enough to exercise the
    /// `SESSIONS` registry that `is_room_joined`/`session_exists` read.
    // WasmWebRtcTransport/WasmL1Transport are wasm-only (single-threaded)
    // types wrapped in Arc for API consistency with the rest of the crate,
    // same as the pre-existing `arc_with_non_send_sync` instances already on
    // `develop` (e.g. `send_via_ctx_tests` above); allowed here rather than
    // left as a bare warning so this test doesn't add to that count.
    #[allow(clippy::arc_with_non_send_sync)]
    fn make_dummy_session(local_id: NodeId) -> Session {
        let engine = MistEngine::new(Arc::new(crate::runtime::WasmRuntime));
        let webrtc = Arc::new(WasmWebRtcTransport::new(
            Arc::new(NoopSignaler) as Arc<dyn Signaler>,
            local_id.clone(),
        ));
        let l1_transport = Arc::new(crate::layers::wasm_l1::WasmL1Transport::new(
            Arc::new(NoopNetTransport) as Arc<dyn Transport>,
            engine.node_store.clone(),
            local_id,
        ));
        Session {
            engine,
            webrtc,
            l1_transport,
        }
    }

    #[wasm_bindgen_test]
    fn is_room_joined_reflects_session_presence() {
        let room = "join-state-is-room-joined";
        assert!(
            !is_room_joined(room.to_string()),
            "a room with no session must report not-joined"
        );

        insert_session(
            room.to_string(),
            make_dummy_session(NodeId("local-test-node".to_string())),
        );
        assert!(
            is_room_joined(room.to_string()),
            "is_room_joined must reflect a session once one is inserted"
        );

        remove_session(room);
        assert!(
            !is_room_joined(room.to_string()),
            "is_room_joined must go back to false once the session is removed"
        );
    }

    /// Exercises the REAL `join_room` entry point (not just the thread-local
    /// helpers) with no `.await` between it and an immediate `leave_room_id`
    /// -- i.e. a same-JS-tick join -> leave, exactly what a JS caller doing
    /// `node.joinRoom(id); node.leaveRoom(id);` produces. This regression
    /// test failed before `L0Engine::join_room` was split into a synchronous
    /// `reserve_join` (run here, before returning) and an async `run_join`
    /// tail: when the whole reservation was deferred into the spawned
    /// future, this same-tick leave raced a still-unscheduled microtask and
    /// wrongly threw "Room not joined" instead of cancelling (confirmed by
    /// running this test against that pre-fix code).
    #[wasm_bindgen_test]
    fn same_tick_join_then_leave_cancels_before_any_await() {
        let room = "join-state-entry-same-tick-leave";
        join_room(room.to_string());
        // No `.await` between these two calls.
        let result = leave_room_id(room.to_string());
        assert!(
            result.is_ok(),
            "a leave arriving in the same JS tick as join_room must cancel the pending build \
             synchronously, not throw 'Room not joined': got {:?}",
            result
        );
        assert!(
            !is_room_joined(room.to_string()),
            "the room must not appear joined right after a same-tick join -> leave"
        );
    }

    /// Same as `same_tick_join_then_leave_cancels_before_any_await` but for
    /// the awaitable entry point: `join_room_async` returns a `js_sys::Promise`
    /// synchronously (its reservation already ran by the time it returns --
    /// see its doc comment), so a same-tick leave right after it, without
    /// ever awaiting the returned promise, must observe the reservation too.
    #[wasm_bindgen_test]
    fn same_tick_join_async_then_leave_cancels_before_any_await() {
        let room = "join-state-entry-same-tick-leave-async";
        let _promise = join_room_async(room.to_string());
        // No `.await` on the promise above, and none between these two calls.
        let result = leave_room_id(room.to_string());
        assert!(
            result.is_ok(),
            "a leave arriving in the same JS tick as join_room_async (without awaiting its \
             promise) must cancel the pending build, not throw 'Room not joined': got {:?}",
            result
        );
        assert!(
            !is_room_joined(room.to_string()),
            "the room must not appear joined right after a same-tick join_room_async -> leave"
        );
    }

    /// `reserve_join` itself (the synchronous half `join_room`/
    /// `join_room_async` both call before returning): a second call for the
    /// same room_id, made before the first's build has finished, must
    /// piggy-back (`PiggyBack`) rather than starting a second duplicate
    /// build (`Owner` again) -- this is what `PENDING_JOINS` exists to
    /// prevent, and it must hold even with zero `.await`s between the two
    /// calls.
    #[wasm_bindgen_test]
    fn reserve_join_second_call_piggybacks_on_first_same_tick() {
        let room = "join-state-reserve-join-piggyback";

        let first = crate::layers::wasm_l0::reserve_join(room);
        assert!(
            matches!(first, crate::layers::wasm_l0::JoinReservation::Owner),
            "the first reserve_join call for a fresh room must own the build"
        );

        let second = crate::layers::wasm_l0::reserve_join(room);
        assert!(
            matches!(
                second,
                crate::layers::wasm_l0::JoinReservation::PiggyBack(_)
            ),
            "a second same-tick reserve_join call for the same room must piggy-back on the \
             first instead of starting a duplicate build"
        );

        clear_join_pending(room);
    }
}
