use std::sync::Arc;

use mistlib_core::signaling::MessageContent;
use mistlib_core::storage::protocol::{classify_storage_frame, StorageFrame};
use mistlib_core::transport::NetworkEvent;
use mistlib_core::types::NodeId;
use tokio::sync::mpsc;

use super::SessionCtx;

impl super::MistEngine {
    /// Pumps `ctx`'s network events until the session is torn down
    /// (`ctx.cancel`) or its transport's event channel closes. Unlike the
    /// single-session original, this no longer re-reads shared engine state
    /// per event: `ctx` *is* the running session for the lifetime of this loop.
    pub(super) async fn process_network_events(
        &self,
        mut rx: mpsc::UnboundedReceiver<NetworkEvent>,
        ctx: Arc<SessionCtx>,
    ) {
        loop {
            tokio::select! {
                _ = ctx.cancel.cancelled() => break,
                event = rx.recv() => {
                    let Some(event) = event else { break };
                    let Some(ov) = ctx.overlay.as_ref() else { continue };

                    let from_origin = event.from.clone();
                    match mistlib_core::overlay::wire::deserialize::<mistlib_core::overlay::OverlayEnvelope>(
                        &event.data,
                    ) {
                        Ok(envelope) => {
                            self.handle_overlay_envelope(envelope, from_origin, &ctx, ov).await;
                        }
                        Err(e) => {
                            tracing::trace!("process_network_events: bincode deserialize failed ({e}), trying storage protocol");
                            // Non-envelope frames are only ever storage protocol;
                            // anything else here is undecodable and never reaches the app.
                            self.handle_storage_message(&event.data, from_origin, &ctx);
                        }
                    }
                }
            }
        }
    }

    async fn handle_overlay_envelope(
        &self,
        envelope: mistlib_core::overlay::OverlayEnvelope,
        from_origin: NodeId,
        ctx: &Arc<SessionCtx>,
        ov: &Arc<mistlib_core::overlay::OverlayRouter>,
    ) {
        ov.learn_route(&envelope.from, &from_origin);
        ctx.touch_node(&from_origin);
        ctx.touch_node(&envelope.from);

        let envelope_from = envelope.from.clone();
        let to_self = envelope.to == *self.self_id.lock().unwrap() || envelope.to.0.is_empty();
        let content = envelope.content.clone();
        let seq = envelope.seq;

        let result = ov.handle_envelope(envelope, from_origin.clone());
        for action in result.actions {
            if let mistlib_core::action::OverlayAction::SendMessage {
                to, data, method, ..
            } = action
            {
                if let Some(wt) = &ctx.webrtc_transport {
                    tokio::select! {
                        _ = ctx.cancel.cancelled() => return,
                        result = wt.enqueue_received_send(&to, data, method) => {
                            if let Err(err) = result {
                                tracing::warn!("Failed to forward overlay action: {err}");
                            }
                        }
                    }
                }
            } else {
                self.handle_action_for(ctx.clone(), action);
            }
        }

        if to_self && result.should_deliver {
            // Restore end-to-end order per source before dispatching (mirrors
            // mistlib-core's engine, mistlib-core/src/engine/network.rs).
            // Keyed and delivered under `envelope_from` (the original overlay
            // sender), not `from_origin` (the immediate transport hop): a
            // relay<->direct route switch changes `from_origin` but not
            // `envelope_from`, and restoring order across exactly that kind
            // of switch is the point of the reorder buffer. `seq == 0`
            // (broadcast/control/legacy) bypasses buffering and delivers
            // immediately, same as the no-overlay path below.
            for ordered in ov.reorder_inbound(&envelope_from, seq, content) {
                self.dispatch_local_message(ordered, envelope_from.clone(), ctx);
            }
        }
    }

    /// Delivers per-source reorder-buffer gaps that timed out without new
    /// traffic arriving to trigger `reorder_inbound`'s lazy flush above (e.g.
    /// the sender went idle after a relay/direct route switch). Called from
    /// this session's periodic background tick (`background.rs`) so a
    /// stalled gap isn't held forever waiting for a message that will never
    /// arrive.
    pub(super) fn flush_expired_reorder(&self, ctx: &Arc<SessionCtx>) {
        let Some(ov) = ctx.overlay.as_ref() else {
            return;
        };
        for (from, contents) in ov.flush_expired_inbound() {
            for content in contents {
                self.dispatch_local_message(content, from.clone(), ctx);
            }
        }
    }

    pub(super) fn dispatch_local_message(
        &self,
        content: MessageContent,
        from_origin: NodeId,
        ctx: &Arc<SessionCtx>,
    ) {
        match content {
            MessageContent::Raw(payload) => {
                if !self.handle_storage_message(&payload, from_origin.clone(), ctx) {
                    super::dispatch_event(super::EVENT_RAW, &ctx.room_id, &from_origin, &payload);
                }
            }
            MessageContent::Overlay(overlay_msg) => {
                if overlay_msg.is_internal_control() {
                    return;
                }
                super::dispatch_event(
                    super::EVENT_OVERLAY,
                    &ctx.room_id,
                    &from_origin,
                    &overlay_msg.payload,
                );
            }
            MessageContent::Data(signaling_data) => {
                ctx.ensure_node_registered(&signaling_data.sender_id);
                if let Some(dispatcher) = &ctx.p2p_signaling_dispatcher {
                    dispatcher
                        .dispatch(self.runtime.handle(), MessageContent::Data(signaling_data));
                } else {
                    tracing::debug!(
                        "NativeEngine: p2p signaling relay not configured, dropping message"
                    );
                }
            }
        }
    }

    /// Handles the storage control protocol (WANT/QUERY/HAVE/...). `ctx` is
    /// the session the message arrived on, threaded through to
    /// `storage::handle_want`/`handle_query` so their HAVE/HAVE_STATUS
    /// replies go back out via the same session's transport (SPEC-15 rule 8).
    fn handle_storage_message(
        &self,
        data: &[u8],
        from_origin: NodeId,
        ctx: &Arc<SessionCtx>,
    ) -> bool {
        let Some(frame) = classify_storage_frame(data) else {
            return false;
        };
        match frame {
            StorageFrame::Want { cid } => {
                crate::storage::enqueue_want(ctx.clone(), from_origin, cid.to_string());
            }
            StorageFrame::Query { cid } => {
                let cid = cid.to_string();
                let ctx = ctx.clone();
                self.runtime.handle().spawn(async move {
                    crate::storage::handle_query(ctx, from_origin, cid).await;
                });
            }
            StorageFrame::HaveStatus { cid } => {
                crate::storage::handle_have_status(from_origin, cid.to_string());
            }
            StorageFrame::Have { cid, data } => {
                crate::storage::WANT_REGISTRY.fulfill_in(
                    &ctx.room_id,
                    cid,
                    &from_origin,
                    data.to_vec(),
                );
            }
            StorageFrame::HaveChunk {
                cid,
                chunk_index,
                chunk_total,
                data,
            } => {
                crate::storage::WANT_REGISTRY.fulfill_chunk_in(
                    &ctx.room_id,
                    cid,
                    &from_origin,
                    chunk_index,
                    chunk_total,
                    data.to_vec(),
                );
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use mistlib_core::config::Config;
    use mistlib_core::error::Result as CoreResult;
    use mistlib_core::overlay::node_store::NodeStore;
    use mistlib_core::overlay::{OverlayEnvelope, OverlayRouter};
    use mistlib_core::signaling::{SignalingData, SignalingHandler, SignalingType};
    use mistlib_core::transport::{NetworkEventHandler, Transport};
    use mistlib_core::types::{ConnectionState, DeliveryMethod};
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Condvar, Mutex as StdMutex};
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

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

    /// Records the `data` field of every `MessageContent::Data` it is handed,
    /// in the order `dispatch_local_message` delivers them, plus a
    /// condvar so tests can block until N deliveries have arrived instead of
    /// polling/sleeping.
    #[derive(Default)]
    struct RecordingSignalingHandler {
        state: StdMutex<Vec<String>>,
        cvar: Condvar,
    }

    #[async_trait]
    impl SignalingHandler for RecordingSignalingHandler {
        async fn handle_message(&self, msg: MessageContent) -> CoreResult<()> {
            if let MessageContent::Data(data) = msg {
                // Make the old one-spawn-per-message implementation fail
                // deterministically: m2 would complete while m1 was sleeping.
                // The ordered dispatcher must not start m2 until m1 completes.
                if data.data == "m1" {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                let mut seen = self.state.lock().unwrap();
                seen.push(data.data);
                self.cvar.notify_all();
            }
            Ok(())
        }
    }

    impl RecordingSignalingHandler {
        /// Blocks until at least `n` messages have been recorded, then
        /// returns everything recorded so far (in delivery order).
        fn wait_for(&self, n: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut seen = self.state.lock().unwrap();
            while seen.len() < n {
                let now = Instant::now();
                assert!(
                    now < deadline,
                    "timed out waiting for {n} recorded messages"
                );
                let (next, timeout) = self
                    .cvar
                    .wait_timeout(seen, deadline.saturating_duration_since(now))
                    .unwrap();
                seen = next;
                assert!(
                    !(timeout.timed_out() && seen.len() < n),
                    "timed out waiting for {n} recorded messages"
                );
            }
            seen.clone()
        }
    }

    /// Builds a fake session with the given overlay router and returns both
    /// the ctx and the concrete recording handler installed as its
    /// `p2p_signaling_dispatcher`, so tests can assert on delivery order without
    /// any trait downcasting.
    fn fake_ctx_with_handler(
        overlay: Arc<OverlayRouter>,
    ) -> (Arc<SessionCtx>, Arc<RecordingSignalingHandler>) {
        let handler = Arc::new(RecordingSignalingHandler::default());
        let ctx = Arc::new(SessionCtx {
            room_id: "room-a".to_string(),
            transport: Arc::new(NoopTransport),
            webrtc_transport: None,
            ws_signaling_handler: Arc::new(NoopSignalingHandler),
            p2p_signaling_dispatcher: Some(Arc::new(super::super::P2pSignalingDispatcher::new(
                handler.clone(),
            ))),
            signaling_dispatch: None,
            bootstrap_signaler: None,
            l1_transport: None,
            l1_notifier: None,
            overlay: Some(overlay),
            node_store: Arc::new(StdMutex::new(NodeStore::new())),
            aoi_nodes: Arc::new(StdMutex::new(HashSet::new())),
            had_connected_peers: AtomicBool::new(false),
            all_connections_lost_dispatched: AtomicBool::new(false),
            cancel: CancellationToken::new(),
        });
        (ctx, handler)
    }

    fn router() -> Arc<OverlayRouter> {
        Arc::new(OverlayRouter::new(
            &Config::new_default(),
            Arc::new(StdMutex::new(NodeStore::new())),
            NodeId("local".to_string()),
        ))
    }

    #[test]
    fn storage_frames_are_handled_without_raw_events_on_both_receive_paths() {
        use mistlib_core::storage::cid::{compute_cid, MULTICODEC_RAW};
        use mistlib_core::storage::protocol::{build_have_chunk_message, build_have_message};

        // dispatch_event uses the process-wide engine; serialize callback changes
        // with the existing event dispatch tests.
        let _guard = crate::events::tests::TEST_LOCK.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        *super::super::ENGINE.rust_event_callback.lock().unwrap() =
            Some(Arc::new(move |kind, from, data| {
                if from == "storage-dispatch-test-peer" {
                    tx.send((kind, data)).unwrap();
                }
            }));

        let engine = super::super::MistEngine::new();
        let (ctx, _) = fake_ctx_with_handler(router());
        let from = NodeId("storage-dispatch-test-peer".into());
        let data = b"storage payload";
        let cid = compute_cid(data, MULTICODEC_RAW);
        let application = b"\x01hello";

        for fallback in [false, true] {
            let registration = crate::storage::WANT_REGISTRY.register_attempt(&cid, from.clone());
            crate::storage::WANT_REGISTRY.track_path(registration.id, &ctx.room_id);
            let mut want = registration.data;
            let frames = [
                build_have_message(&cid, data),
                build_have_chunk_message(&cid, 0, 1, data),
                application.to_vec(),
            ];
            if fallback {
                let (tx, network_rx) = mpsc::unbounded_channel();
                // Undecodable non-storage frames on this path never reach the app.
                for frame in frames {
                    assert!(
                        mistlib_core::overlay::wire::deserialize::<OverlayEnvelope>(&frame)
                            .is_err()
                    );
                    tx.send(NetworkEvent {
                        from: from.clone(),
                        data: frame.into(),
                    })
                    .unwrap();
                }
                drop(tx);
                engine
                    .runtime
                    .block_on(engine.process_network_events(network_rx, ctx.clone()));
                // Barrier: an app frame through the envelope path is the only event.
                engine.dispatch_local_message(
                    MessageContent::Raw(application.to_vec().into()),
                    from.clone(),
                    &ctx,
                );
            } else {
                for frame in frames {
                    engine.dispatch_local_message(
                        MessageContent::Raw(frame.into()),
                        from.clone(),
                        &ctx,
                    );
                }
            }
            assert_eq!(want.try_recv().unwrap(), data);
            // The app event is a FIFO barrier: any leaked storage event would
            // already be ahead of it in the dispatch queue.
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                (super::super::EVENT_RAW, application.to_vec())
            );
            assert!(rx.try_recv().is_err());
        }

        *super::super::ENGINE.rust_event_callback.lock().unwrap() = None;
    }

    /// Builds a unicast `ReliableOrdered` envelope from `peer-a` to `local`
    /// carrying a tagged `MessageContent::Data`, with a distinct `msg_id` per
    /// call (the dedup cache keys on `(from, msg_id)`, so reusing one would
    /// make the second envelope look like a duplicate and get dropped before
    /// ever reaching the reorder buffer).
    fn envelope(seq: u64, msg_id: u64, tag: &str) -> OverlayEnvelope {
        OverlayEnvelope {
            from: NodeId("peer-a".to_string()),
            to: NodeId("local".to_string()),
            msg_id,
            seq,
            hop_count: 1,
            content: MessageContent::Data(SignalingData {
                sender_id: NodeId("peer-a".to_string()),
                receiver_id: NodeId("local".to_string()),
                room_id: "room-a".to_string(),
                data: tag.to_string(),
                signaling_type: SignalingType::Offer,
            }),
        }
    }

    // Plain `#[test]` + `engine.runtime.block_on(...)` (the crate's own
    // convention for driving async engine code from a sync caller, e.g.
    // `ENGINE.runtime.block_on(...)` in app.rs/room.rs) rather than
    // `#[tokio::test]`: each test builds its own `MistEngine`, which owns a
    // real nested `tokio::runtime::Runtime`; dropping that at the end of an
    // `async fn` driven by a *different* ambient runtime is the classic
    // "Cannot drop a runtime in an asynchronous context" panic. Driving it
    // with its own `block_on` from a sync test avoids that entirely.
    #[test]
    fn out_of_order_unicast_reaches_local_dispatch_in_order() {
        let engine = super::super::MistEngine::new();
        let ov = router();
        let (ctx, handler) = fake_ctx_with_handler(ov.clone());
        let from_origin = NodeId("peer-a".to_string());

        engine.runtime.block_on(async {
            // seq 2 arrives first: it must be buffered, not delivered yet.
            engine
                .handle_overlay_envelope(envelope(2, 2, "m2"), from_origin.clone(), &ctx, &ov)
                .await;
            // seq 1 arrives second: it unblocks both, in order (1, 2).
            engine
                .handle_overlay_envelope(envelope(1, 1, "m1"), from_origin, &ctx, &ov)
                .await;
        });

        assert_eq!(handler.wait_for(2), vec!["m1", "m2"]);
    }

    #[test]
    fn zero_seq_message_passes_through_unbuffered() {
        let engine = super::super::MistEngine::new();
        let ov = router();
        let (ctx, handler) = fake_ctx_with_handler(ov.clone());
        let from_origin = NodeId("peer-a".to_string());

        engine.runtime.block_on(engine.handle_overlay_envelope(
            envelope(0, 1, "ctrl"),
            from_origin,
            &ctx,
            &ov,
        ));

        assert_eq!(handler.wait_for(1), vec!["ctrl"]);
    }

    #[test]
    fn broadcast_message_passes_through_unbuffered() {
        let engine = super::super::MistEngine::new();
        let ov = router();
        let (ctx, handler) = fake_ctx_with_handler(ov.clone());
        let from_origin = NodeId("peer-a".to_string());

        let mut env = envelope(0, 1, "bcast");
        env.to = NodeId(String::new());
        engine
            .runtime
            .block_on(engine.handle_overlay_envelope(env, from_origin, &ctx, &ov));

        assert_eq!(handler.wait_for(1), vec!["bcast"]);
    }
}
