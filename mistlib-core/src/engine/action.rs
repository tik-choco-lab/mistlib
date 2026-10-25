use super::{EngineState, MistEngine, RunningContext};
use crate::action::{OverlayAction, SendLane};
use crate::overlay::ActionHandler;
use crate::transport::send::{
    closed_send, pipeline_enabled, shrink_permit, ByteBudget, BytePermit, LaneQueue, ScheduledSend,
    SendClass, SendFuture, SendReservation,
};
use crate::types::{DeliveryMethod, NodeId};
use bytes::Bytes;
use futures_util::{stream::FuturesUnordered, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, Notify};

pub(super) struct PendingSend {
    ctx: Arc<RunningContext>,
    data: Bytes,
    method: DeliveryMethod,
    class: SendClass,
    generation: Option<u64>,
    completion: Option<oneshot::Sender<crate::error::Result<()>>>,
    _telemetry: Option<crate::transport::send::QueueCharge>,
    _budget: Option<BytePermit>,
    budget_wait: Option<SendFuture<'static, crate::error::Result<BytePermit>>>,
}
impl ScheduledSend for PendingSend {
    fn reliable_unordered(&self) -> bool {
        self.method == DeliveryMethod::ReliableUnordered
    }
    fn send_class(&self) -> SendClass {
        self.class
    }
}

struct QueueState {
    frames: LaneQueue<PendingSend>,
    running: bool,
    reservations: usize,
    closed: bool,
}
pub(super) struct PeerQueue {
    generation: Option<u64>,
    telemetry: Arc<crate::transport::send::QueueTelemetry>,
    state: Mutex<QueueState>,
    budget: Option<Arc<ByteBudget>>,
    changed: Notify,
}
struct QueueLease(Arc<PeerQueue>);
impl Drop for QueueLease {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().reservations -= 1;
        self.0.changed.notify_one();
    }
}
pub(super) type SendQueues = Arc<Mutex<HashMap<NodeId, Arc<PeerQueue>>>>;

impl ActionHandler for MistEngine {
    fn handle_action(&self, action: OverlayAction) {
        if matches!(action, OverlayAction::SendMessage { .. }) {
            // This callback also accepts frames already stamped by callers.
            // Retain them synchronously in FIFO order; the worker awaits budget.
            if let Err(err) = self.retain_send(action) {
                tracing::warn!("Failed to admit overlay send: {err}");
            }
            return;
        }
        let state_arc = self.state.clone();

        self.runtime.spawn(Box::pin(async move {
            let ctx = {
                let lock = state_arc.lock().expect("state lock poisoned");
                if let EngineState::Running(c) = &*lock {
                    Some(c.clone())
                } else {
                    None
                }
            };
            let Some(ctx) = ctx else { return };

            match action {
                OverlayAction::SendMessage { .. } => unreachable!("sends use the ordered queue"),
                OverlayAction::Connect { to } => {
                    if let Err(e) = ctx.preferred_transport().connect(&to).await {
                        tracing::warn!("Failed to connect to {}: {:?}", to.0, e);
                    }
                }
                OverlayAction::Disconnect { to } => {
                    if let Err(e) = ctx.preferred_transport().disconnect(&to).await {
                        tracing::warn!("Failed to disconnect from {}: {:?}", to.0, e);
                    }
                }
                OverlayAction::SuspectDisconnected { to } => {
                    if let Err(e) = ctx.preferred_transport().suspect_disconnected(&to).await {
                        tracing::warn!("Failed to mark {} as suspect-disconnected: {:?}", to.0, e);
                    }
                }
                OverlayAction::ClearSuspect { to } => {
                    if let Err(e) = ctx.preferred_transport().clear_suspect(&to).await {
                        tracing::warn!("Failed to clear suspect state for {}: {:?}", to.0, e);
                    }
                }
                OverlayAction::SendSignaling { to, envelope } => {
                    if let Some(sig) = &ctx.signaling_dispatch {
                        if let Err(e) = sig.send_signaling(&to, envelope.content).await {
                            tracing::warn!("Failed to send signaling to {}: {:?}", to.0, e);
                        }
                    }
                }
            }
        }));
    }

    fn try_dispatch_send(&self, action: OverlayAction) -> crate::error::Result<()> {
        let OverlayAction::SendMessage {
            to,
            data,
            method,
            lane,
        } = action
        else {
            self.handle_action(action);
            return Ok(());
        };
        let ctx = self.running_context().ok_or_else(closed_send)?;
        let generation = ctx.preferred_transport().send_generation(&to);
        let lease = self.send_queue(&to, &ctx);
        let budget = if lane == SendLane::Bulk {
            lease
                .0
                .budget
                .as_ref()
                .map(|b| b.try_reserve(data.len()))
                .transpose()?
        } else {
            None
        };
        self.admit_send(
            to,
            lease,
            PendingSend {
                ctx,
                generation,
                class: SendClass::from_wire(&data),
                data,
                method,
                completion: None,
                _telemetry: None,
                _budget: budget,
                budget_wait: None,
            },
        )
    }

    fn reserve_send<'a>(
        &'a self,
        to: &'a NodeId,
        bytes: usize,
        lane: SendLane,
    ) -> SendFuture<'a, crate::error::Result<SendReservation>> {
        Box::pin(async move {
            let ctx = self.running_context().ok_or_else(closed_send)?;
            let generation = ctx.preferred_transport().send_generation(to);
            let lease = self.send_queue(to, &ctx);
            let mut budget = if lane == SendLane::Bulk {
                if let Some(budget) = &lease.0.budget {
                    Some(budget.reserve(bytes).await?)
                } else {
                    None
                }
            } else {
                None
            };
            if lease.0.state.lock().unwrap().closed
                || ctx.preferred_transport().send_generation(to) != generation
            {
                return Err(closed_send());
            }
            let to = to.clone();
            // Own everything needed to commit; no await or router re-entry.
            let queues = self.send_queues.clone();
            let runtime = self.runtime.clone();
            let engine_state = self.state.clone();
            Ok(Box::new(move |action| {
                let OverlayAction::SendMessage {
                    to: actual,
                    data,
                    method,
                    ..
                } = action
                else {
                    return Err(closed_send());
                };
                if actual != to {
                    return Err(closed_send());
                }
                shrink_permit(&mut budget, data.len());
                let (tx, rx) = oneshot::channel();
                Self::admit_to_queue(
                    queues,
                    runtime,
                    engine_state,
                    to,
                    lease,
                    PendingSend {
                        ctx,
                        generation,
                        class: SendClass::from_wire(&data),
                        data,
                        method,
                        completion: Some(tx),
                        _telemetry: None,
                        _budget: budget,
                        budget_wait: None,
                    },
                )?;
                Ok(rx)
            }) as SendReservation)
        })
    }
}

impl MistEngine {
    fn retain_send(&self, action: OverlayAction) -> crate::error::Result<()> {
        let OverlayAction::SendMessage {
            to, data, method, ..
        } = action
        else {
            unreachable!()
        };
        let ctx = self.running_context().ok_or_else(closed_send)?;
        let generation = ctx.preferred_transport().send_generation(&to);
        let lease = self.send_queue(&to, &ctx);
        let class = SendClass::from_wire(&data);
        let mut budget = None;
        let mut budget_wait = None;
        if class.lane == SendLane::Bulk {
            if let Some(b) = &lease.0.budget {
                let mut wait = b.reserve_received_owned(data.len());
                // Register with the fair semaphore now, before later local
                // reservations can occupy budget behind this FIFO barrier.
                use std::task::{Context, Poll};
                match wait.as_mut().poll(&mut Context::from_waker(
                    futures_util::task::noop_waker_ref(),
                )) {
                    Poll::Ready(result) => budget = Some(result?),
                    Poll::Pending => budget_wait = Some(wait),
                }
            }
        }
        self.admit_send(
            to,
            lease,
            PendingSend {
                ctx,
                generation,
                class,
                data,
                method,
                completion: None,
                _telemetry: None,
                _budget: budget,
                budget_wait,
            },
        )
    }

    /// Backpressure the receive pump before consuming another relay frame.
    pub(super) async fn dispatch_received_action(&self, action: OverlayAction) {
        let OverlayAction::SendMessage {
            to,
            data,
            method,
            lane,
        } = action
        else {
            self.handle_action(action);
            return;
        };
        let result = async {
            let ctx = self.running_context().ok_or_else(closed_send)?;
            let generation = ctx.preferred_transport().send_generation(&to);
            let lease = self.send_queue(&to, &ctx);
            let budget = if lane == SendLane::Bulk {
                match &lease.0.budget {
                    Some(b) => Some(b.reserve_received(data.len()).await?),
                    None => None,
                }
            } else {
                None
            };
            if ctx.preferred_transport().send_generation(&to) != generation {
                return Err(closed_send());
            }
            self.admit_send(
                to,
                lease,
                PendingSend {
                    ctx,
                    generation,
                    class: SendClass::from_wire(&data),
                    data,
                    method,
                    completion: None,
                    _telemetry: None,
                    _budget: budget,
                    budget_wait: None,
                },
            )
        }
        .await;
        if let Err(err) = result {
            tracing::warn!("Failed to forward overlay action: {err}");
        }
    }

    fn send_queue(&self, to: &NodeId, ctx: &RunningContext) -> QueueLease {
        let transfer = ctx
            .overlay
            .as_ref()
            .map(|r| r.transfer.clone())
            .unwrap_or_default();
        let generation = ctx.preferred_transport().send_generation(to);
        let mut queues = self.send_queues.lock().unwrap();
        if queues
            .get(to)
            .is_some_and(|queue| queue.generation != generation)
        {
            if let Some(old) = queues.remove(to) {
                if let Some(budget) = &old.budget {
                    budget.close();
                }
                old.changed.notify_one();
            }
        }
        // Prune idle queues left by cancelled, uncommitted reservations.
        queues.retain(|_, queue| {
            let state = queue.state.lock().unwrap();
            state.running || state.reservations != 0
        });
        let queue = queues
            .entry(to.clone())
            .or_insert_with(|| {
                Arc::new(PeerQueue {
                    generation,
                    telemetry: Arc::new(Default::default()),
                    state: Mutex::new(QueueState {
                        frames: LaneQueue::new(transfer.control_priority),
                        running: false,
                        reservations: 0,
                        closed: false,
                    }),
                    budget: ctx
                        .preferred_transport()
                        .bulk_admission_budget(to)
                        .or_else(|| {
                            pipeline_enabled(&transfer)
                                .then(|| Arc::new(ByteBudget::new(transfer.bulk_queue_bytes)))
                        }),
                    changed: Notify::new(),
                })
            })
            .clone();
        queue.state.lock().unwrap().reservations += 1;
        QueueLease(queue)
    }

    fn admit_send(
        &self,
        to: NodeId,
        lease: QueueLease,
        pending: PendingSend,
    ) -> crate::error::Result<()> {
        Self::admit_to_queue(
            self.send_queues.clone(),
            self.runtime.clone(),
            self.state.clone(),
            to,
            lease,
            pending,
        )
    }

    fn admit_to_queue(
        queues: SendQueues,
        runtime: Arc<dyn crate::runtime::AsyncRuntime>,
        engine_state: Arc<Mutex<EngineState>>,
        to: NodeId,
        lease: QueueLease,
        mut pending: PendingSend,
    ) -> crate::error::Result<()> {
        let queue = lease.0.clone();
        pending._telemetry = queue
            .telemetry
            .charge(pending.data.len(), pending.class.lane);
        let start = {
            let mut state = queue.state.lock().unwrap();
            if state.closed {
                return Err(closed_send());
            }
            state.frames.push_back(pending);
            let start = !state.running;
            state.running = true;
            start
        };
        queue.changed.notify_one();
        drop(lease);
        if !start {
            return Ok(());
        }
        runtime.spawn(Box::pin(async move {
            let mut writes: FuturesUnordered<SendFuture<'static, bool>> = FuturesUnordered::new();
            let (mut ordered_busy, mut bulk_busy) = (false, false);
            loop {
                let changed = queue.changed.notified();
                {
                    let mut map = queues.lock().unwrap();
                    let state = queue.state.lock().unwrap();
                    if state.closed { return; }
                    if state.frames.is_empty() && writes.is_empty() && state.reservations == 0 {
                        if map.get(&to).is_some_and(|current| Arc::ptr_eq(current, &queue)) { map.remove(&to); }
                        return;
                    }
                }
                loop {
                    let next = queue.state.lock().unwrap().frames.pop(!ordered_busy, !bulk_busy);
                    let Some((pending, bulk)) = next else { break };
                    if bulk { bulk_busy = true; } else { ordered_busy = true; }
                    let node = to.clone();
                    let current = engine_state.clone();
                    writes.push(Box::pin(async move {
                        let PendingSend { ctx, data, method, class, generation, completion, mut _budget, budget_wait, _telemetry } = pending;
                        let capacity = if let Some(wait) = budget_wait {
                            wait.await.map(|permit| { _budget = Some(permit); })
                        } else { Ok(()) };
                        let valid = matches!(&*current.lock().unwrap(), EngineState::Running(active) if Arc::ptr_eq(active, &ctx))
                            && ctx.preferred_transport().send_generation(&node) == generation;
                        let result = if let Err(err) = capacity { Err(err) } else if !valid { Err(closed_send()) } else {
                            let transport = ctx.preferred_transport();
                            if node.is_broadcast() { transport.broadcast(data, method).await }
                            else { transport.send_in_lane(&node, data, method, class.lane, completion.is_some()).await }
                        };
                        if result.is_err() && completion.is_none() && class.lane == SendLane::Bulk {
                            crate::storage::telemetry::STORAGE_COUNTERS.responder_write_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        if let Err(err) = &result { tracing::warn!(destination = %node, error = %err, "Overlay: worker write failed"); }
                        if let Some(tx) = completion { let _ = tx.send(result); }
                        drop(_budget);
                        drop(_telemetry);
                        bulk
                    }));
                }
                if writes.is_empty() { changed.await; }
                else {
                    use futures_util::FutureExt;
                    let completed = writes.next().fuse();
                    let changed = changed.fuse();
                    futures_util::pin_mut!(completed, changed);
                    futures_util::select! {
                        bulk = completed => {
                            if bulk == Some(true) { bulk_busy = false; } else { ordered_busy = false; }
                        },
                        _ = changed => {},
                    }
                }
            }
        }));
        Ok(())
    }

    pub(super) fn close_send_queues(&self) {
        for (_, queue) in self.send_queues.lock().unwrap().drain() {
            let mut state = queue.state.lock().unwrap();
            state.closed = true;
            state.frames = LaneQueue::new(false);
            if let Some(budget) = &queue.budget {
                budget.close();
            }
            queue.changed.notify_one();
        }
    }
}

impl MistEngine {
    /// Snapshot only; queue locks are released before the caller does any I/O.
    pub fn bulk_queue_stats(&self) -> HashMap<String, (u64, Option<u32>)> {
        self.send_queues
            .lock()
            .unwrap()
            .iter()
            .map(|(node, queue)| {
                (
                    node.0.clone(),
                    (
                        queue.telemetry.queued_bytes(),
                        queue.budget.as_ref().map(|b| b.limit()),
                    ),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::overlay::node_store::NodeStore;
    use crate::overlay::{OverlayEnvelope, OverlayRouter, OverlayTransport};
    use crate::runtime::{AsyncRuntime, BoxTask};
    use crate::signaling::{MessageContent, SignalingHandler};
    use crate::transport::{NetworkEventHandler, Transport};
    use crate::types::ConnectionState;
    use async_trait::async_trait;
    use std::task::{Context, Poll};

    #[derive(Default)]
    struct ManualRuntime(Mutex<Vec<BoxTask>>);

    #[async_trait]
    impl AsyncRuntime for ManualRuntime {
        fn spawn(&self, task: BoxTask) {
            self.0.lock().unwrap().push(task);
        }
        async fn sleep(&self, _duration: web_time::Duration) {
            unreachable!("no timers needed");
        }
    }

    struct RecordingTransport {
        generation: std::sync::atomic::AtomicU64,
        sequences: Mutex<Vec<(NodeId, u64)>>,
        pause: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    #[async_trait]
    impl Transport for RecordingTransport {
        async fn start(&self, _handler: Arc<dyn NetworkEventHandler>) -> crate::error::Result<()> {
            Ok(())
        }
        async fn send(
            &self,
            node: &NodeId,
            data: Bytes,
            _method: DeliveryMethod,
        ) -> crate::error::Result<()> {
            if node.0 == "peer" {
                let pause = self.pause.lock().unwrap().take();
                if let Some(pause) = pause {
                    pause.await.unwrap();
                }
            }
            let envelope: OverlayEnvelope = crate::overlay::wire::deserialize(&data).unwrap();
            self.sequences
                .lock()
                .unwrap()
                .push((node.clone(), envelope.seq));
            Ok(())
        }
        fn send_generation(&self, _: &NodeId) -> Option<u64> {
            Some(self.generation.load(std::sync::atomic::Ordering::SeqCst))
        }
        async fn broadcast(
            &self,
            _data: Bytes,
            _method: DeliveryMethod,
        ) -> crate::error::Result<()> {
            unreachable!("unicast test");
        }
        fn get_connection_state(&self, _node: &NodeId) -> ConnectionState {
            ConnectionState::Connected
        }
        async fn connect(&self, _node: &NodeId) -> crate::error::Result<()> {
            Ok(())
        }
        async fn disconnect(&self, _node: &NodeId) -> crate::error::Result<()> {
            Ok(())
        }
        fn get_connected_nodes(&self) -> Vec<NodeId> {
            Vec::new()
        }
    }

    struct NoopSignaling;
    #[async_trait]
    impl SignalingHandler for NoopSignaling {
        async fn handle_message(&self, _msg: MessageContent) -> crate::error::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn received_bulk_waits_for_capacity_without_losing_or_renumbering_frames() {
        for seq in [0, 1] {
            for synchronous_callback in [false, true] {
                let runtime = Arc::new(ManualRuntime::default());
                let engine = MistEngine::new(runtime.clone());
                let (resume, pause) = oneshot::channel();
                let raw = Arc::new(RecordingTransport {
                    generation: std::sync::atomic::AtomicU64::new(1),
                    sequences: Mutex::new(Vec::new()),
                    pause: Mutex::new(Some(pause)),
                });
                let mut config = Config::default();
                config.storage.transfer.control_priority = true;
                config.storage.transfer.bulk_queue_bytes = 1024;
                let router = Arc::new(OverlayRouter::new(
                    &config,
                    Arc::new(Mutex::new(NodeStore::new())),
                    NodeId("relay".into()),
                ));
                *engine.state.lock().unwrap() = EngineState::Running(Arc::new(RunningContext {
                    transport: raw.clone(),
                    network_transport: None,
                    signaling_handler: Arc::new(NoopSignaling),
                    p2p_signaling_handler: None,
                    signaling_dispatch: None,
                    websocket_signaler: None,
                    overlay: Some(router),
                }));
                let cid = crate::storage::compute_cid(b"data", 0x55);
                let action = |seq| OverlayAction::SendMessage {
                    to: NodeId("peer".into()),
                    method: DeliveryMethod::ReliableOrdered,
                    lane: SendLane::Bulk,
                    data: crate::overlay::wire::serialize(
                        &OverlayEnvelope::new(
                            NodeId("source".into()),
                            NodeId("destination".into()),
                            2,
                            MessageContent::Raw(
                                crate::storage::protocol::build_have_chunk_message(
                                    &cid, 0, 1, &[0; 800],
                                )
                                .into(),
                            ),
                        )
                        .with_seq(seq),
                    )
                    .unwrap()
                    .into(),
                };
                engine.try_dispatch_send(action(seq)).unwrap();
                let mut worker = runtime.0.lock().unwrap().pop().unwrap();
                assert!(futures::poll!(worker.as_mut()).is_pending());
                let second_seq = if seq == 0 { 0 } else { 2 };
                let mut admission = Box::pin(engine.dispatch_received_action(action(second_seq)));
                if synchronous_callback {
                    engine.handle_action(action(second_seq));
                } else {
                    assert!(futures::poll!(admission.as_mut()).is_pending());
                }
                // A later local send cannot steal capacity ahead of the retained frame.
                assert!(engine.try_dispatch_send(action(3)).is_err());
                assert!(raw.sequences.lock().unwrap().is_empty());
                resume.send(()).unwrap();
                let finished = futures::poll!(worker.as_mut()).is_ready();
                if !synchronous_callback {
                    admission.await;
                }
                if !finished {
                    worker.await;
                }
                assert_eq!(
                    *raw.sequences.lock().unwrap(),
                    vec![
                        (NodeId("peer".into()), seq),
                        (NodeId("peer".into()), second_seq)
                    ]
                );
            }
        }
    }

    #[test]
    fn send_workers_preserve_fifo_across_backpressure_and_reverse_task_poll_order() {
        let runtime = Arc::new(ManualRuntime::default());
        let engine = MistEngine::new(runtime.clone());
        let (resume, pause) = tokio::sync::oneshot::channel();
        let raw = Arc::new(RecordingTransport {
            generation: std::sync::atomic::AtomicU64::new(1),
            sequences: Mutex::new(Vec::new()),
            pause: Mutex::new(Some(pause)),
        });
        let router = Arc::new(OverlayRouter::new(
            &Config::default(),
            Arc::new(Mutex::new(NodeStore::new())),
            NodeId("sender".into()),
        ));
        *engine.state.lock().unwrap() = EngineState::Running(Arc::new(RunningContext {
            transport: raw.clone(),
            network_transport: None,
            signaling_handler: Arc::new(NoopSignaling),
            p2p_signaling_handler: None,
            signaling_dispatch: None,
            websocket_signaler: None,
            overlay: Some(router.clone()),
        }));
        let overlay = OverlayTransport {
            router,
            action_handler: engine.clone(),
        };
        let peer = NodeId("peer".into());
        let other = NodeId("other".into());
        let send = |to: &NodeId| {
            futures::executor::block_on(overlay.send(
                to,
                Bytes::new(),
                DeliveryMethod::ReliableOrdered,
            ))
            .unwrap()
        };
        for _ in 0..10 {
            send(&peer);
        }
        send(&other);
        let mut workers = std::mem::take(&mut *runtime.0.lock().unwrap());
        assert_eq!(workers.len(), 2, "one worker per peer, not per frame");
        // Poll in reverse spawn order. The other peer must make progress while
        // the first peer is stalled, without polling later frames ahead of it.
        futures::executor::block_on(workers.pop().unwrap());
        let mut worker = workers.pop().unwrap();
        let waker = futures::task::noop_waker();
        assert_eq!(
            worker.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        );
        assert_eq!(*raw.sequences.lock().unwrap(), vec![(other.clone(), 1)]);
        send(&peer);
        assert!(
            runtime.0.lock().unwrap().is_empty(),
            "stalled peer keeps its existing worker"
        );
        resume.send(()).unwrap();
        futures::executor::block_on(worker);
        let recorded = raw.sequences.lock().unwrap();
        assert_eq!(
            &recorded[1..],
            &(1..=11).map(|seq| (peer.clone(), seq)).collect::<Vec<_>>()
        );
        assert!(engine.send_queues.lock().unwrap().is_empty());
    }
    #[test]
    fn control_writer_runs_while_zero_bulk_is_blocked_and_bulk_budget_stays_charged() {
        let runtime = Arc::new(ManualRuntime::default());
        let engine = MistEngine::new(runtime.clone());
        let (resume, pause) = tokio::sync::oneshot::channel();
        let raw = Arc::new(RecordingTransport {
            generation: std::sync::atomic::AtomicU64::new(1),
            sequences: Mutex::new(Vec::new()),
            pause: Mutex::new(Some(pause)),
        });
        let mut config = Config::default();
        config.storage.transfer.unsequenced_bulk = true;
        config.storage.transfer.control_priority = true;
        config.storage.transfer.bulk_queue_bytes = 1024;
        let router = Arc::new(OverlayRouter::new(
            &config,
            Arc::new(Mutex::new(NodeStore::new())),
            NodeId("sender".into()),
        ));
        *engine.state.lock().unwrap() = EngineState::Running(Arc::new(RunningContext {
            transport: raw.clone(),
            network_transport: None,
            signaling_handler: Arc::new(NoopSignaling),
            p2p_signaling_handler: None,
            signaling_dispatch: None,
            websocket_signaler: None,
            overlay: Some(router.clone()),
        }));
        let overlay = OverlayTransport {
            router,
            action_handler: engine.clone(),
        };
        let peer = NodeId("peer".into());
        let cid = crate::storage::compute_cid(b"data", 0x55);
        let chunk = Bytes::from(crate::storage::protocol::build_have_chunk_message(
            &cid, 0, 1, &[0; 800],
        ));
        futures::executor::block_on(overlay.send(
            &peer,
            chunk.clone(),
            DeliveryMethod::ReliableOrdered,
        ))
        .unwrap();
        let mut worker = runtime.0.lock().unwrap().pop().unwrap();
        let waker = futures::task::noop_waker();
        assert_eq!(
            worker.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        );
        // The frame has left the queue but its active write still owns budget.
        assert!(futures::executor::block_on(overlay.send(
            &peer,
            chunk,
            DeliveryMethod::ReliableOrdered
        ))
        .is_err());
        futures::executor::block_on(overlay.send(
            &peer,
            Bytes::from(crate::storage::protocol::build_want_message(&cid)),
            DeliveryMethod::ReliableOrdered,
        ))
        .unwrap();
        assert_eq!(
            worker.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        );
        assert_eq!(*raw.sequences.lock().unwrap(), vec![(peer.clone(), 1)]);
        resume.send(()).unwrap();
        futures::executor::block_on(worker);
        assert_eq!(
            *raw.sequences.lock().unwrap(),
            vec![(peer.clone(), 1), (peer.clone(), 0)]
        );
        futures::executor::block_on(overlay.send(
            &peer,
            Bytes::from_static(b"stale"),
            DeliveryMethod::ReliableOrdered,
        ))
        .unwrap();
        raw.generation.store(2, std::sync::atomic::Ordering::SeqCst);
        let stale_worker = runtime.0.lock().unwrap().pop().unwrap();
        futures::executor::block_on(stale_worker);
        assert_eq!(
            raw.sequences.lock().unwrap().len(),
            2,
            "old admitted sends cannot replay onto replacement PCs"
        );
    }
}
