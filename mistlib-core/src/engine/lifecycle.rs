use super::{EngineState, MistEngine, RunningContext};
use crate::signaling::MessageContent;
use crate::transport::{NetworkEvent, NetworkEventHandler};
use std::collections::HashSet;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::oneshot;

/// Upper bound on how long `run()` waits for a previous `leave_room()`
/// cleanup to finish before starting the new session anyway.
const CLEANUP_WAIT_TIMEOUT_MS: u64 = 5000;

struct NetworkEventSender(mpsc::UnboundedSender<NetworkEvent>);

impl NetworkEventHandler for NetworkEventSender {
    fn on_event(&self, event: NetworkEvent) {
        let _ = self.0.send(event);
    }
}

impl MistEngine {
    pub fn leave_room(&self) {
        self.run_generation.fetch_add(1, AtomicOrdering::Relaxed);

        let (transport_opt, network_transport_opt, websocket_signaler_opt, to_disconnect) = {
            let state = self.state.lock().expect("state lock poisoned");
            if let EngineState::Running(ctx) = &*state {
                let mut nodes: HashSet<_> =
                    ctx.transport.get_connected_nodes().into_iter().collect();
                if let Some(nt) = &ctx.network_transport {
                    nodes.extend(nt.get_connected_nodes());
                }
                (
                    Some(ctx.transport.clone()),
                    ctx.network_transport.clone(),
                    ctx.websocket_signaler.clone(),
                    nodes,
                )
            } else {
                (None, None, None, HashSet::new())
            }
        };

        if let Some(transport) = transport_opt {
            let (done_tx, done_rx) = oneshot::channel();
            *self
                .cleanup_done
                .lock()
                .expect("cleanup_done lock poisoned") = Some(done_rx);

            self.runtime.spawn(Box::pin(async move {
                if let Some(ws) = websocket_signaler_opt {
                    if let Err(err) = ws.close().await {
                        tracing::warn!("leave_room: websocket close failed: {:?}", err);
                    }
                }
                for node in to_disconnect {
                    if let Err(e) = transport.disconnect(&node).await {
                        tracing::warn!("leave_room: disconnect failed for {}: {:?}", node.0, e);
                    }
                    if let Some(nt) = &network_transport_opt {
                        if let Err(e) = nt.disconnect(&node).await {
                            tracing::warn!(
                                "leave_room: network disconnect failed for {}: {:?}",
                                node.0,
                                e
                            );
                        }
                    }
                }
                // Best-effort: a rejoin that already timed out its wait may
                // have dropped the receiver, in which case this is ignored.
                let _ = done_tx.send(());
            }));
        }

        *self.state.lock().expect("state lock poisoned") = EngineState::Idle;

        let mut store = self.node_store.lock().expect("node_store lock poisoned");
        store.nodes.clear();
        store.last_updated.clear();
        self.aoi_nodes
            .lock()
            .expect("aoi_nodes lock poisoned")
            .clear();
    }

    pub async fn run(
        self: Arc<Self>,
        ctx: RunningContext,
        mut sig_rx: mpsc::UnboundedReceiver<MessageContent>,
    ) -> Result<(), String> {
        self.await_previous_cleanup().await;

        let run_generation = self
            .run_generation
            .fetch_add(1, AtomicOrdering::Relaxed)
            .wrapping_add(1);
        let (tx, mut rx) = mpsc::unbounded_channel::<NetworkEvent>();

        let ctx_arc = Arc::new(ctx);
        *self.state.lock().expect("state lock poisoned") = EngineState::Running(ctx_arc.clone());

        if let Some(nt) = &ctx_arc.network_transport {
            nt.start(Arc::new(NetworkEventSender(tx.clone())))
                .await
                .map_err(|e| e.to_string())?;
        }
        ctx_arc
            .transport
            .start(Arc::new(NetworkEventSender(tx)))
            .await
            .map_err(|e| e.to_string())?;

        let ctx_sig = ctx_arc.clone();
        let self_sig = self.clone();
        self.runtime.spawn(Box::pin(async move {
            loop {
                if self_sig.run_generation.load(AtomicOrdering::Relaxed) != run_generation {
                    break;
                }
                let msg = match sig_rx.try_recv() {
                    Ok(msg) => msg,
                    Err(TryRecvError::Empty) => {
                        self_sig
                            .runtime
                            .sleep(web_time::Duration::from_millis(100))
                            .await;
                        continue;
                    }
                    Err(TryRecvError::Disconnected) => break,
                };
                if let Err(err) = ctx_sig.signaling_handler.handle_message(msg).await {
                    tracing::warn!("MistEngine: signaling handler failed: {:?}", err);
                }
            }
        }));

        let self_net = self.clone();
        self.runtime.spawn(Box::pin(async move {
            loop {
                if self_net.run_generation.load(AtomicOrdering::Relaxed) != run_generation {
                    break;
                }
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(TryRecvError::Empty) => {
                        self_net
                            .runtime
                            .sleep(web_time::Duration::from_millis(100))
                            .await;
                        continue;
                    }
                    Err(TryRecvError::Disconnected) => break,
                };
                self_net.process_network_event(event).await;
            }
        }));

        let self_tick = self.clone();
        self.runtime.spawn(Box::pin(async move {
            loop {
                self_tick
                    .runtime
                    .sleep(web_time::Duration::from_millis(1000))
                    .await;
                if self_tick.run_generation.load(AtomicOrdering::Relaxed) != run_generation {
                    break;
                }
                self_tick.tick().await;
            }
        }));

        Ok(())
    }

    /// Waits for a previous `leave_room()`'s disconnect cleanup to finish, so
    /// its in-flight disconnects can't race a newly established connection to
    /// the same node in this session. Gives up after
    /// `CLEANUP_WAIT_TIMEOUT_MS` and proceeds anyway.
    async fn await_previous_cleanup(&self) {
        let Some(done_rx) = self
            .cleanup_done
            .lock()
            .expect("cleanup_done lock poisoned")
            .take()
        else {
            return;
        };

        let timeout = self
            .runtime
            .sleep(web_time::Duration::from_millis(CLEANUP_WAIT_TIMEOUT_MS));
        match futures_util::future::select(done_rx, timeout).await {
            // Ok(()) = cleanup finished; Err(Closed) = the cleanup task was
            // dropped. Either way there is nothing left to wait for.
            futures_util::future::Either::Left(_) => {}
            futures_util::future::Either::Right(_) => {
                tracing::warn!(
                    "MistEngine::run: previous leave_room cleanup did not finish within {}ms; starting new session anyway",
                    CLEANUP_WAIT_TIMEOUT_MS
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result as MistResult;
    use crate::runtime::{AsyncRuntime, BoxTask};
    use crate::signaling::{Signaler, SignalingHandler};
    use crate::transport::{NetworkEventHandler, Transport};
    use crate::types::{ConnectionState, DeliveryMethod, NodeId};
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::sync::Mutex as StdMutex;

    // mistlib-core only pulls in tokio's "sync" feature (no "rt"/"time"), so
    // this test runtime drives spawned futures on their own OS thread via
    // `futures::executor::block_on` rather than a tokio runtime.
    struct TestRuntime;

    #[async_trait]
    impl AsyncRuntime for TestRuntime {
        fn spawn(&self, future: BoxTask) {
            std::thread::spawn(move || {
                futures::executor::block_on(future);
            });
        }
        async fn sleep(&self, duration: web_time::Duration) {
            std::thread::sleep(duration);
        }
    }

    /// Records call order across the leave/run race so the test can assert
    /// the cleanup actually finished before the new session started.
    #[derive(Default)]
    struct CallLog(StdMutex<Vec<&'static str>>);

    impl CallLog {
        fn record(&self, event: &'static str) {
            self.0.lock().expect("call log lock poisoned").push(event);
        }
        fn snapshot(&self) -> Vec<&'static str> {
            self.0.lock().expect("call log lock poisoned").clone()
        }
    }

    struct NoopTransport;

    #[async_trait]
    impl Transport for NoopTransport {
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

    /// A transport whose `start()` records when it actually ran, so the test
    /// can check it happened after the previous session's cleanup finished.
    struct RecordingTransport(Arc<CallLog>);

    #[async_trait]
    impl Transport for RecordingTransport {
        async fn start(&self, _handler: Arc<dyn NetworkEventHandler>) -> MistResult<()> {
            self.0.record("second_session_transport_start");
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

    struct NoopSignalingHandler;

    #[async_trait]
    impl SignalingHandler for NoopSignalingHandler {
        async fn handle_message(&self, _msg: MessageContent) -> MistResult<()> {
            Ok(())
        }
    }

    /// A websocket signaler whose `close()` takes a while, standing in for a
    /// slow disconnect during `leave_room()`'s cleanup.
    struct SlowClosingSignaler(Arc<CallLog>);

    #[async_trait]
    impl Signaler for SlowClosingSignaler {
        async fn send_signaling(&self, _to: &NodeId, _msg: MessageContent) -> MistResult<()> {
            Ok(())
        }
        async fn close(&self) -> MistResult<()> {
            std::thread::sleep(std::time::Duration::from_millis(200));
            self.0.record("previous_session_cleanup_done");
            Ok(())
        }
    }

    fn make_ctx(
        transport: Arc<dyn Transport>,
        websocket_signaler: Option<Arc<dyn Signaler>>,
    ) -> RunningContext {
        RunningContext {
            transport,
            network_transport: None,
            signaling_handler: Arc::new(NoopSignalingHandler),
            p2p_signaling_handler: None,
            signaling_dispatch: None,
            websocket_signaler,
            overlay: None,
        }
    }

    #[test]
    fn run_waits_for_previous_leave_room_cleanup_before_starting() {
        futures::executor::block_on(async {
            let log = Arc::new(CallLog::default());
            let engine = MistEngine::new(Arc::new(TestRuntime));

            // Seed a Running state so leave_room() has something to clean up.
            let first_ctx = make_ctx(
                Arc::new(NoopTransport),
                Some(Arc::new(SlowClosingSignaler(log.clone()))),
            );
            *engine.state.lock().expect("state lock poisoned") =
                EngineState::Running(Arc::new(first_ctx));

            engine.leave_room();

            // Immediately start a new session, racing the in-flight cleanup.
            let second_ctx = make_ctx(Arc::new(RecordingTransport(log.clone())), None);
            let (_sig_tx, sig_rx) = mpsc::unbounded_channel();
            engine
                .clone()
                .run(second_ctx, sig_rx)
                .await
                .expect("run should succeed");

            assert_eq!(
                log.snapshot(),
                vec![
                    "previous_session_cleanup_done",
                    "second_session_transport_start"
                ],
                "run() must wait for the previous leave_room() cleanup to finish \
                 before starting the transport for the new session"
            );
        });
    }

    #[test]
    fn run_proceeds_immediately_when_there_is_no_pending_cleanup() {
        futures::executor::block_on(async {
            let log = Arc::new(CallLog::default());
            let engine = MistEngine::new(Arc::new(TestRuntime));

            let ctx = make_ctx(Arc::new(RecordingTransport(log.clone())), None);
            let (_sig_tx, sig_rx) = mpsc::unbounded_channel();

            let start = web_time::Instant::now();
            engine
                .clone()
                .run(ctx, sig_rx)
                .await
                .expect("run should succeed");

            assert!(
                start.elapsed() < web_time::Duration::from_millis(100),
                "run() should not wait when there is no previous cleanup pending"
            );
            assert_eq!(log.snapshot(), vec!["second_session_transport_start"]);
        });
    }
}
