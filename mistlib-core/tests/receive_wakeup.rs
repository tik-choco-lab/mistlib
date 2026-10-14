#![cfg(all(feature = "sim", not(target_arch = "wasm32")))]

use async_trait::async_trait;
use bytes::Bytes;
use mistlib_core::engine::{EngineEvent, EngineEventHandler, MistEngine, RunningContext};
use mistlib_core::error::Result;
use mistlib_core::runtime::{AsyncRuntime, BoxTask};
use mistlib_core::signaling::{MessageContent, SignalingHandler};
use mistlib_core::transport::{NetworkEvent, NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{advance, Duration, Instant};

// The existing sim feature supplies Tokio's paused clock. No wall-clock sleeps
// or threads are needed: data arrival must wake the actual engine receiver
// while its virtual 100 ms idle timer is still pending.
#[derive(Default)]
struct Runtime(Mutex<Vec<JoinHandle<()>>>);

#[async_trait]
impl AsyncRuntime for Runtime {
    fn spawn(&self, future: BoxTask) {
        self.0.lock().unwrap().push(tokio::spawn(future));
    }
    async fn sleep(&self, duration: web_time::Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[derive(Default)]
struct TestTransport(Mutex<Option<Arc<dyn NetworkEventHandler>>>);

impl TestTransport {
    fn receive(&self, payload: &'static [u8]) {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .on_event(NetworkEvent {
                from: NodeId("remote".into()),
                data: Bytes::from_static(payload),
            });
    }
}

#[async_trait]
impl Transport for TestTransport {
    async fn start(&self, handler: Arc<dyn NetworkEventHandler>) -> Result<()> {
        *self.0.lock().unwrap() = Some(handler);
        Ok(())
    }
    async fn send(&self, _: &NodeId, _: Bytes, _: DeliveryMethod) -> Result<()> {
        Ok(())
    }
    async fn broadcast(&self, _: Bytes, _: DeliveryMethod) -> Result<()> {
        Ok(())
    }
    async fn connect(&self, _: &NodeId) -> Result<()> {
        Ok(())
    }
    async fn disconnect(&self, _: &NodeId) -> Result<()> {
        Ok(())
    }
    fn get_connection_state(&self, _: &NodeId) -> ConnectionState {
        ConnectionState::Connected
    }
    fn get_connected_nodes(&self) -> Vec<NodeId> {
        Vec::new()
    }
}

struct Signaling;

#[async_trait]
impl SignalingHandler for Signaling {
    async fn handle_message(&self, _: MessageContent) -> Result<()> {
        Ok(())
    }
}

struct Events(mpsc::UnboundedSender<Bytes>);

impl EngineEventHandler for Events {
    fn on_event(&self, event: EngineEvent) {
        if let EngineEvent::RawMessage(_, payload) = event {
            self.0.send(payload).unwrap();
        }
    }
}

async fn start_receiver(
    engine: &Arc<MistEngine>,
    runtime: &Runtime,
    transport: Arc<TestTransport>,
) -> JoinHandle<()> {
    let (_sig_tx, sig_rx) = mpsc::unbounded_channel();
    engine
        .clone()
        .run(
            RunningContext {
                transport,
                network_transport: None,
                signaling_handler: Arc::new(Signaling),
                p2p_signaling_handler: None,
                signaling_dispatch: None,
                websocket_signaler: None,
                overlay: None,
            },
            sig_rx,
        )
        .await
        .unwrap();
    // run spawns signaling, network, then periodic-tick tasks. Isolate the
    // network task so other timers cannot advance this test's paused clock.
    let mut tasks = std::mem::take(&mut *runtime.0.lock().unwrap());
    assert_eq!(tasks.len(), 3);
    let receiver = tasks.remove(1);
    for task in tasks {
        task.abort();
    }
    tokio::task::yield_now().await;
    receiver
}

async fn setup() -> (
    Arc<MistEngine>,
    Arc<Runtime>,
    Arc<TestTransport>,
    mpsc::UnboundedReceiver<Bytes>,
    JoinHandle<()>,
) {
    let runtime = Arc::new(Runtime::default());
    let engine = MistEngine::new(runtime.clone());
    let transport = Arc::new(TestTransport::default());
    let (tx, events) = mpsc::unbounded_channel();
    engine.set_event_handler(Arc::new(Events(tx)));
    let receiver = start_receiver(&engine, &runtime, transport.clone()).await;
    (engine, runtime, transport, events, receiver)
}

#[tokio::test(start_paused = true)]
async fn network_arrival_wakes_receiver_before_idle_timer() {
    let (_engine, _runtime, transport, mut events, receiver) = setup().await;
    let start = Instant::now();
    advance(Duration::from_millis(1)).await;
    transport.receive(b"fresh");
    tokio::task::yield_now().await;
    assert_eq!(events.try_recv().unwrap(), Bytes::from_static(b"fresh"));
    assert_eq!(start.elapsed(), Duration::from_millis(1));
    receiver.abort();
}

#[tokio::test(start_paused = true)]
async fn idle_leave_stops_receiver_with_sender_still_open() {
    let (engine, _runtime, transport, mut events, receiver) = setup().await;
    engine.leave_room_and_wait().await;
    assert!(transport.0.lock().unwrap().is_some(), "sender remains open");
    assert!(!receiver.is_finished());
    advance(Duration::from_millis(100)).await;
    tokio::task::yield_now().await;
    assert!(
        receiver.is_finished(),
        "idle task must notice leave within 100 ms"
    );
    receiver.await.unwrap();
    transport.receive(b"after leave");
    assert!(events.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn leave_and_rejoin_suppress_event_queued_for_old_receiver() {
    let (engine, runtime, old_transport, mut events, old_receiver) = setup().await;
    // Queue data while the receiver is awaiting recv, but change generations
    // before allowing that task to resume. This exercises the post-await guard.
    old_transport.receive(b"old session");
    engine.leave_room_and_wait().await;
    let new_transport = Arc::new(TestTransport::default());
    let new_receiver = start_receiver(&engine, &runtime, new_transport.clone()).await;
    assert!(old_receiver.is_finished());
    old_receiver.await.unwrap();
    assert!(
        events.try_recv().is_err(),
        "old event must not reach new session"
    );
    new_transport.receive(b"new session");
    tokio::task::yield_now().await;
    assert_eq!(
        events.try_recv().unwrap(),
        Bytes::from_static(b"new session")
    );
    new_receiver.abort();
}

#[tokio::test(start_paused = true)]
async fn closing_network_channel_stops_receiver_without_timer_wait() {
    let (_engine, _runtime, transport, _events, receiver) = setup().await;
    let start = Instant::now();
    transport.0.lock().unwrap().take();
    tokio::task::yield_now().await;
    assert!(receiver.is_finished());
    receiver.await.unwrap();
    assert_eq!(start.elapsed(), Duration::ZERO);
}
