use super::*;
use mistlib_core::stats::STATS;
use mistlib_core::types::NodeId;

#[tokio::test]
async fn test_engine_get_stats_json_structure() {
    STATS.add_send(100);
    STATS.add_receive(50);
    STATS.add_world_send(200);
    STATS.add_relay_send(25);
    STATS.set_rtt(NodeId("peer-1".to_string()), 15.5);

    let stats_json = ENGINE.get_stats_json().await;
    let stats: serde_json::Value = serde_json::from_str(&stats_json).unwrap();

    assert_eq!(stats["sendBits"], 100 * 8);
    assert_eq!(stats["receiveBits"], 50 * 8);
    assert_eq!(stats["worldSendBits"], 200 * 8);
    assert_eq!(stats["relaySendBits"], 25 * 8);
    assert_eq!(stats["rttMillis"]["peer-1"], 15.5);
    assert!(stats["nodes"].is_array());

    let next_stats_json = ENGINE.get_stats_json().await;
    let next_stats: serde_json::Value = serde_json::from_str(&next_stats_json).unwrap();
    assert_eq!(next_stats["sendBits"], 0);
}

/// SPEC-15 multi-room session registry lifecycle. These exercise
/// `MistEngine`'s registry API directly (rather than the full
/// `join_room`/`leave_room` FFI path) with network-free session stacks:
/// `join_room`/`leave_room`/`leave_room_id` are thin orchestration around
/// exactly this API (see `layers/native_l0/room.rs`), and going through the
/// real path would mean actually dialing `Config::new_default()`'s live
/// signaling endpoint from a unit test.
// `pub(crate)` (module and the handful of items below it, not every test
// function): `storage::tests` (SPEC-16's `EngineSessionPositions`) also
// mutates `ENGINE`'s session registry and needs to serialize against these
// tests via the SAME lock, plus a minimal fake session to attach a
// `NodeStore` to -- see `storage.rs`'s `self_position_source_tests`.
pub(crate) mod session_registry {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use mistlib_core::error::Result as CoreResult;
    use mistlib_core::signaling::{MessageContent, SignalingHandler};
    use mistlib_core::transport::{NetworkEventHandler, Transport};
    use mistlib_core::types::{ConnectionState, DeliveryMethod};
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::sync::{LazyLock, Mutex as StdMutex};
    use tokio::sync::Mutex as AsyncMutex;
    use tokio_util::sync::CancellationToken;

    // `ENGINE` is one process-wide static, so tests that mutate its session
    // registry must not run concurrently with each other (no other test in
    // this crate touches `ENGINE.sessions`, but these would touch each
    // other). An async mutex, not `std::sync::Mutex`, because the guard is
    // held across the `.await`s below -- a std guard there would trip
    // `clippy::await_holding_lock` (and could deadlock a single-threaded
    // executor).
    pub(crate) static REGISTRY_TEST_LOCK: LazyLock<AsyncMutex<()>> =
        LazyLock::new(|| AsyncMutex::new(()));

    struct NoopTransport;
    #[async_trait]
    impl Transport for NoopTransport {
        async fn start(&self, _handler: Arc<dyn NetworkEventHandler>) -> CoreResult<()> {
            Ok(())
        }
        async fn send(
            &self,
            _node: &NodeId,
            _data: Bytes,
            _method: DeliveryMethod,
        ) -> CoreResult<()> {
            Ok(())
        }
        async fn broadcast(&self, _data: Bytes, _method: DeliveryMethod) -> CoreResult<()> {
            Ok(())
        }
        fn get_connection_state(&self, _node: &NodeId) -> ConnectionState {
            ConnectionState::Disconnected
        }
        async fn connect(&self, _node: &NodeId) -> CoreResult<()> {
            Ok(())
        }
        async fn disconnect(&self, _node: &NodeId) -> CoreResult<()> {
            Ok(())
        }
        fn get_connected_nodes(&self) -> Vec<NodeId> {
            vec![]
        }
    }

    struct NoopSignalingHandler;
    #[async_trait]
    impl SignalingHandler for NoopSignalingHandler {
        async fn handle_message(&self, _msg: MessageContent) -> CoreResult<()> {
            Ok(())
        }
    }

    /// A minimal, network-free session: no WebRTC transport, no signaler --
    /// just enough for the registry itself to be exercised.
    pub(crate) fn fake_session(room_id: &str) -> Arc<SessionCtx> {
        Arc::new(SessionCtx {
            room_id: room_id.to_string(),
            transport: Arc::new(NoopTransport),
            webrtc_transport: None,
            ws_signaling_handler: Arc::new(NoopSignalingHandler),
            p2p_signaling_handler: None,
            signaling_dispatch: None,
            bootstrap_signaler: None,
            l1_transport: None,
            l1_notifier: None,
            overlay: None,
            node_store: Arc::new(StdMutex::new(NodeStore::new())),
            aoi_nodes: Arc::new(StdMutex::new(HashSet::new())),
            had_connected_peers: AtomicBool::new(false),
            all_connections_lost_dispatched: AtomicBool::new(false),
            cancel: CancellationToken::new(),
        })
    }

    #[tokio::test]
    async fn join_a_and_join_b_coexist() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        ENGINE.remove_all_sessions().await;

        assert!(
            ENGINE
                .insert_session("room-a".to_string(), fake_session("room-a"))
                .await
        );
        assert!(
            ENGINE
                .insert_session("room-b".to_string(), fake_session("room-b"))
                .await
        );

        assert!(ENGINE.has_session("room-a").await);
        assert!(ENGINE.has_session("room-b").await);
        assert_eq!(ENGINE.sessions_snapshot().await.len(), 2);

        ENGINE.remove_all_sessions().await;
    }

    #[tokio::test]
    async fn leave_room_id_keeps_other_rooms_running() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        ENGINE.remove_all_sessions().await;

        ENGINE
            .insert_session("room-a".to_string(), fake_session("room-a"))
            .await;
        ENGINE
            .insert_session("room-b".to_string(), fake_session("room-b"))
            .await;

        let removed = ENGINE.remove_session("room-a").await;
        assert!(
            removed.is_some(),
            "leave_room_id should return the removed session"
        );

        assert!(!ENGINE.has_session("room-a").await);
        assert!(
            ENGINE.has_session("room-b").await,
            "room-b must stay active"
        );
        assert_eq!(ENGINE.sessions_snapshot().await.len(), 1);

        ENGINE.remove_all_sessions().await;
    }

    #[tokio::test]
    async fn leave_room_clears_every_active_session() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        ENGINE.remove_all_sessions().await;

        ENGINE
            .insert_session("room-a".to_string(), fake_session("room-a"))
            .await;
        ENGINE
            .insert_session("room-b".to_string(), fake_session("room-b"))
            .await;

        let removed = ENGINE.remove_all_sessions().await;
        assert_eq!(removed.len(), 2);

        assert!(ENGINE.sessions_snapshot().await.is_empty());
        assert!(!ENGINE.has_session("room-a").await);
        assert!(!ENGINE.has_session("room-b").await);
    }

    #[tokio::test]
    async fn double_join_of_the_same_room_keeps_exactly_one_session() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        ENGINE.remove_all_sessions().await;

        let first = fake_session("room-a");
        let second = fake_session("room-a");

        assert!(
            ENGINE
                .insert_session("room-a".to_string(), first.clone())
                .await
        );
        assert!(
            !ENGINE.insert_session("room-a".to_string(), second).await,
            "a second insert for an already-active room must be rejected"
        );

        let sessions = ENGINE.sessions_snapshot().await;
        assert_eq!(sessions.len(), 1);
        assert!(
            Arc::ptr_eq(&sessions[0].1, &first),
            "the original session must be the one still registered"
        );

        ENGINE.remove_all_sessions().await;
    }

    #[tokio::test]
    async fn primary_session_is_the_first_joined() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        ENGINE.remove_all_sessions().await;

        let first = fake_session("room-a");
        ENGINE
            .insert_session("room-a".to_string(), first.clone())
            .await;
        ENGINE
            .insert_session("room-b".to_string(), fake_session("room-b"))
            .await;

        let primary = ENGINE.primary_session().await.expect("a session is active");
        assert!(Arc::ptr_eq(&primary, &first));

        ENGINE.remove_all_sessions().await;
    }

    /// A session whose `webrtc_transport` is `wt` instead of `None` --
    /// everything else is the same network-free stub as `fake_session`.
    /// Needed by `handle_action_for`'s `SendMessage` ordering regression
    /// test (`super::action_ordering`), which needs a real, connected
    /// `WebRtcTransport` behind a `SessionCtx` it can hand to
    /// `ENGINE.handle_action_for`.
    pub(crate) fn session_with_webrtc_transport(
        room_id: &str,
        wt: Arc<crate::transports::WebRtcTransport>,
    ) -> Arc<SessionCtx> {
        Arc::new(SessionCtx {
            room_id: room_id.to_string(),
            transport: wt.clone(),
            webrtc_transport: Some(wt.clone()),
            ws_signaling_handler: wt,
            p2p_signaling_handler: None,
            signaling_dispatch: None,
            bootstrap_signaler: None,
            l1_transport: None,
            l1_notifier: None,
            overlay: None,
            node_store: Arc::new(StdMutex::new(NodeStore::new())),
            aoi_nodes: Arc::new(StdMutex::new(HashSet::new())),
            had_connected_peers: AtomicBool::new(false),
            all_connections_lost_dispatched: AtomicBool::new(false),
            cancel: CancellationToken::new(),
        })
    }
}

/// Regression coverage for the self-inflicted reordering bug described in
/// `docs/REORDER_RELIABILITY_NOTES.md`: `handle_action_for` used to spawn an
/// independent `tokio::task` per `OverlayAction::SendMessage`, so N
/// sequential (non-concurrent) calls from the *same* caller -- exactly how
/// `SessionActionHandler::handle_action` (`layers/native_l0/init.rs`) is
/// actually invoked, one action at a time, in the order `OverlayRouter`
/// produced them -- could still have their underlying `dc.send()` calls
/// execute out of order, since `tokio::spawn` makes no ordering guarantee
/// between independently spawned tasks. This exercises that exact call
/// pattern: `ENGINE.handle_action_for` called back-to-back, synchronously, no
/// spawning at the call site.
mod action_ordering {
    use super::session_registry::{session_with_webrtc_transport, REGISTRY_TEST_LOCK};
    use super::*;
    use crate::transports::webrtc::tests::disconnect::{make_connected_pair, wait_for_state};
    use bytes::Bytes;
    use mistlib_core::action::OverlayAction;
    use mistlib_core::transport::{NetworkEvent, NetworkEventHandler, Transport};
    use mistlib_core::types::{ConnectionState, DeliveryMethod};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    struct OrderRecorder {
        seen: StdMutex<Vec<u32>>,
    }
    impl NetworkEventHandler for OrderRecorder {
        fn on_event(&self, event: NetworkEvent) {
            if event.data.len() != 4 {
                return;
            }
            let n = u32::from_be_bytes(event.data[..4].try_into().unwrap());
            self.seen.lock().unwrap().push(n);
        }
    }
    struct NoopEventHandler;
    impl NetworkEventHandler for NoopEventHandler {
        fn on_event(&self, _event: NetworkEvent) {}
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sequential_send_actions_preserve_order() {
        // Establish the real connection BEFORE touching `ENGINE` at all.
        // `ENGINE` is a process-wide `LazyLock`; if this test is the first
        // thing in the process to dereference it, first-time construction
        // (`MistEngine::new`) synchronously builds an entire second Tokio
        // multi-thread runtime plus an OS dispatch thread. Doing that at the
        // same time as this sandbox's already-marginal real (local) UDP ICE
        // connectivity check was observed to noticeably raise this test's
        // connect-timeout flakiness; deferring every `ENGINE` access until
        // after the pair is already `Connected` avoids stacking that cost on
        // top of the timing-sensitive handshake window.
        let (ta, tb, id_a, id_b) = make_connected_pair();
        let recorder = Arc::new(OrderRecorder {
            seen: StdMutex::new(Vec::new()),
        });
        // Handlers must be wired before `connect()` -- `create_pc`/
        // `setup_outgoing_data_channels` snapshot `event_handler` at
        // data-channel-creation time.
        ta.start(Arc::new(NoopEventHandler)).await.unwrap();
        tb.start(recorder.clone()).await.unwrap();

        ta.connect(&id_b).await.expect("connect should not fail");
        assert!(
            wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
            "A did not reach Connected state"
        );
        assert!(
            wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
            "B did not reach Connected state"
        );

        let _guard = REGISTRY_TEST_LOCK.lock().await;
        ENGINE.remove_all_sessions().await;
        let room_id = "action-ordering-test".to_string();
        let ctx = session_with_webrtc_transport(&room_id, ta.clone());
        assert!(ENGINE.insert_session(room_id.clone(), ctx.clone()).await);

        // The real call pattern this reproduces: `SessionActionHandler::handle_action`
        // is called synchronously, once per action, in the order `OverlayRouter`
        // produced them -- never concurrently from independently spawned
        // callers. No `tokio::spawn` here at all, deliberately.
        const COUNT: u32 = 200;
        for i in 0..COUNT {
            ENGINE.handle_action_for(
                ctx.clone(),
                OverlayAction::SendMessage {
                    to: id_b.clone(),
                    data: Bytes::copy_from_slice(&i.to_be_bytes()),
                    method: DeliveryMethod::ReliableOrdered,
                },
            );
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if recorder.seen.lock().unwrap().len() as u32 >= COUNT {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        ENGINE.remove_all_sessions().await;

        let seen = recorder.seen.lock().unwrap().clone();
        let expected: Vec<u32> = (0..COUNT).collect();
        assert_eq!(
            seen.len(),
            COUNT as usize,
            "not all messages arrived within the deadline"
        );
        assert_eq!(
            seen, expected,
            "sequential handle_action_for(SendMessage) calls must preserve send order -- \
             if this fails, a spawn was reintroduced somewhere between \
             SessionActionHandler::handle_action and the DataChannel write"
        );
    }
}
