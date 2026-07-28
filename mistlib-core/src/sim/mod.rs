use crate::error::{MistError, Result};
use crate::transport::{NetworkEvent, NetworkEventHandler, Transport};
use crate::types::{ConnectionState, DeliveryMethod, NodeId};
use async_trait::async_trait;
use bytes::Bytes;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

pub mod scenario;
mod signaling;

pub use scenario::{run_scenario, ScenarioConfig};
pub use signaling::{SimSignaler, SimSignalingHub};

const DEFAULT_INBOX_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkCondition {
    pub latency: Duration,
    pub jitter: Duration,
    pub loss_rate: f64,
}

impl Default for LinkCondition {
    fn default() -> Self {
        Self {
            latency: Duration::ZERO,
            jitter: Duration::ZERO,
            loss_rate: 0.0,
        }
    }
}

impl LinkCondition {
    pub fn new(latency: Duration, jitter: Duration, loss_rate: f64) -> Self {
        Self {
            latency,
            jitter,
            loss_rate: loss_rate.clamp(0.0, 1.0),
        }
    }
}

pub struct SimNetwork {
    inboxes: RwLock<HashMap<NodeId, mpsc::Sender<NetworkEvent>>>,
    links: RwLock<HashMap<(NodeId, NodeId), LinkCondition>>,
    link_queues: Mutex<HashMap<(NodeId, NodeId), LinkQueue>>,
    default_condition: RwLock<LinkCondition>,
    rng: Mutex<StdRng>,
    inbox_capacity: usize,
}

impl SimNetwork {
    pub fn new(seed: u64) -> Self {
        Self::with_inbox_capacity(seed, DEFAULT_INBOX_CAPACITY)
    }

    pub fn with_inbox_capacity(seed: u64, inbox_capacity: usize) -> Self {
        Self {
            inboxes: RwLock::new(HashMap::new()),
            links: RwLock::new(HashMap::new()),
            link_queues: Mutex::new(HashMap::new()),
            default_condition: RwLock::new(LinkCondition::default()),
            rng: Mutex::new(StdRng::seed_from_u64(seed)),
            inbox_capacity,
        }
    }

    pub fn set_default_condition(&self, condition: LinkCondition) {
        *self.default_condition.write().unwrap() = condition;
    }

    pub fn set_link_condition(&self, from: NodeId, to: NodeId, condition: LinkCondition) {
        self.links.write().unwrap().insert((from, to), condition);
    }

    pub fn remove_link_condition(&self, from: &NodeId, to: &NodeId) -> Option<LinkCondition> {
        self.links
            .write()
            .unwrap()
            .remove(&(from.clone(), to.clone()))
    }

    pub fn transport(self: &Arc<Self>, node_id: NodeId) -> SimTransport {
        let (tx, rx) = mpsc::channel(self.inbox_capacity);
        self.inboxes.write().unwrap().insert(node_id.clone(), tx);
        SimTransport {
            node_id,
            net: Arc::clone(self),
            inbox: Mutex::new(Some(rx)),
            connections: Mutex::new(HashSet::new()),
        }
    }

    fn inbox_for(&self, node: &NodeId) -> Option<mpsc::Sender<NetworkEvent>> {
        self.inboxes.read().unwrap().get(node).cloned()
    }

    fn registered_nodes(&self) -> Vec<NodeId> {
        self.inboxes.read().unwrap().keys().cloned().collect()
    }

    fn link_condition(&self, from: &NodeId, to: &NodeId) -> LinkCondition {
        self.links
            .read()
            .unwrap()
            .get(&(from.clone(), to.clone()))
            .copied()
            .unwrap_or_else(|| *self.default_condition.read().unwrap())
    }

    fn delivery_sample(&self, condition: LinkCondition, method: DeliveryMethod) -> DeliverySample {
        let mut rng = self.rng.lock().unwrap();
        let dropped = matches!(
            method,
            DeliveryMethod::Unreliable | DeliveryMethod::UnreliableOrdered
        ) && condition.loss_rate > 0.0
            && rng.gen::<f64>() < condition.loss_rate;
        let jitter = if condition.jitter.is_zero() {
            Duration::ZERO
        } else {
            Duration::from_nanos(rng.gen_range(0..=duration_nanos_u64(condition.jitter)))
        };
        DeliverySample {
            dropped,
            delay: condition.latency.saturating_add(jitter),
        }
    }

    fn enqueue_delivery(
        &self,
        from: NodeId,
        to: NodeId,
        mut pending: PendingDelivery,
    ) -> Result<()> {
        let mut queues = self.link_queues.lock().unwrap();
        let queue = queues.entry((from.clone(), to.clone())).or_insert_with(|| {
            let (tx, mut rx) = mpsc::unbounded_channel::<PendingDelivery>();
            tokio::spawn(async move {
                while let Some(pending) = rx.recv().await {
                    if pending.sample.dropped {
                        continue;
                    }
                    tokio::time::sleep_until(pending.deliver_at).await;
                    let _ = pending
                        .tx
                        .send(NetworkEvent {
                            from: pending.from,
                            data: pending.data,
                        })
                        .await;
                }
            });
            LinkQueue {
                tx,
                last_deliver_at: None,
            }
        });

        if !pending.sample.dropped {
            if let Some(last_deliver_at) = queue.last_deliver_at {
                pending.deliver_at = pending.deliver_at.max(last_deliver_at);
            }
            queue.last_deliver_at = Some(pending.deliver_at);
        }

        queue
            .tx
            .send(pending)
            .map_err(|_| MistError::Network(format!("sim link queue closed for {from}->{to}")))
    }

    async fn deliver(
        &self,
        from: NodeId,
        to: NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> Result<()> {
        let tx = self
            .inbox_for(&to)
            .ok_or_else(|| MistError::NodeNotFound(to.clone()))?;
        let sample = self.delivery_sample(self.link_condition(&from, &to), method);
        let deliver_at = Instant::now() + sample.delay;
        self.enqueue_delivery(
            from.clone(),
            to.clone(),
            PendingDelivery {
                from: from.clone(),
                data,
                tx,
                sample,
                deliver_at,
            },
        )
    }
}

struct LinkQueue {
    tx: mpsc::UnboundedSender<PendingDelivery>,
    last_deliver_at: Option<Instant>,
}

struct DeliverySample {
    dropped: bool,
    delay: Duration,
}

struct PendingDelivery {
    from: NodeId,
    data: Bytes,
    tx: mpsc::Sender<NetworkEvent>,
    sample: DeliverySample,
    deliver_at: Instant,
}

pub struct SimTransport {
    node_id: NodeId,
    net: Arc<SimNetwork>,
    inbox: Mutex<Option<mpsc::Receiver<NetworkEvent>>>,
    connections: Mutex<HashSet<NodeId>>,
}

impl SimTransport {
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }
}

#[async_trait]
impl Transport for SimTransport {
    async fn start(&self, handler: Arc<dyn NetworkEventHandler>) -> Result<()> {
        let mut inbox = self.inbox.lock().unwrap().take().ok_or_else(|| {
            MistError::Internal(format!(
                "sim transport for {} already started",
                self.node_id
            ))
        })?;
        tokio::spawn(async move {
            while let Some(event) = inbox.recv().await {
                handler.on_event(event);
            }
        });
        Ok(())
    }

    async fn send(&self, node: &NodeId, data: Bytes, method: DeliveryMethod) -> Result<()> {
        if self.get_connection_state(node) != ConnectionState::Connected {
            return Err(MistError::Network(format!(
                "sim transport {} is not connected to {node}",
                self.node_id
            )));
        }
        self.net
            .deliver(self.node_id.clone(), node.clone(), data, method)
            .await
    }

    async fn broadcast(&self, data: Bytes, method: DeliveryMethod) -> Result<()> {
        for node in self.get_connected_nodes() {
            self.send(&node, data.clone(), method).await?;
        }
        Ok(())
    }

    fn get_connection_state(&self, node: &NodeId) -> ConnectionState {
        if self.connections.lock().unwrap().contains(node) {
            ConnectionState::Connected
        } else {
            ConnectionState::Disconnected
        }
    }

    async fn connect(&self, node: &NodeId) -> Result<()> {
        if self.net.inbox_for(node).is_none() {
            return Err(MistError::NodeNotFound(node.clone()));
        }
        if *node == self.node_id {
            return Err(MistError::Network(format!(
                "sim transport {} cannot connect to itself",
                self.node_id
            )));
        }
        self.connections.lock().unwrap().insert(node.clone());
        Ok(())
    }

    async fn disconnect(&self, node: &NodeId) -> Result<()> {
        self.connections.lock().unwrap().remove(node);
        Ok(())
    }

    fn get_connected_nodes(&self) -> Vec<NodeId> {
        let registered: HashSet<_> = self.net.registered_nodes().into_iter().collect();
        self.connections
            .lock()
            .unwrap()
            .iter()
            .filter(|node| registered.contains(*node))
            .cloned()
            .collect()
    }
}

fn duration_nanos_u64(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::time::{timeout, Instant};

    struct RecordingHandler {
        events: StdMutex<Vec<NetworkEvent>>,
    }

    impl RecordingHandler {
        fn new() -> Self {
            Self {
                events: StdMutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<NetworkEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl NetworkEventHandler for RecordingHandler {
        fn on_event(&self, event: NetworkEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    struct TimedRecordingHandler {
        events: StdMutex<Vec<(NetworkEvent, Instant)>>,
    }

    impl TimedRecordingHandler {
        fn new() -> Self {
            Self {
                events: StdMutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<(NetworkEvent, Instant)> {
            self.events.lock().unwrap().clone()
        }
    }

    impl NetworkEventHandler for TimedRecordingHandler {
        fn on_event(&self, event: NetworkEvent) {
            self.events.lock().unwrap().push((event, Instant::now()));
        }
    }

    fn node(id: &str) -> NodeId {
        NodeId(id.to_string())
    }

    #[tokio::test]
    async fn sends_and_receives_between_connected_nodes() {
        let net = Arc::new(SimNetwork::new(7));
        let a = net.transport(node("a"));
        let b = net.transport(node("b"));
        let handler = Arc::new(RecordingHandler::new());

        b.start(handler.clone()).await.unwrap();
        a.connect(b.node_id()).await.unwrap();
        a.send(
            b.node_id(),
            Bytes::from_static(b"hello"),
            DeliveryMethod::ReliableOrdered,
        )
        .await
        .unwrap();

        timeout(Duration::from_millis(100), async {
            loop {
                if !handler.events().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let events = handler.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].from, node("a"));
        assert_eq!(events[0].data, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn loss_applies_to_unreliable_delivery_methods() {
        let net = Arc::new(SimNetwork::new(11));
        net.set_default_condition(LinkCondition::new(Duration::ZERO, Duration::ZERO, 1.0));
        let a = net.transport(node("a"));
        let b = net.transport(node("b"));
        let handler = Arc::new(RecordingHandler::new());

        b.start(handler.clone()).await.unwrap();
        a.connect(b.node_id()).await.unwrap();
        a.send(
            b.node_id(),
            Bytes::from_static(b"drop-unordered"),
            DeliveryMethod::Unreliable,
        )
        .await
        .unwrap();
        a.send(
            b.node_id(),
            Bytes::from_static(b"drop-ordered"),
            DeliveryMethod::UnreliableOrdered,
        )
        .await
        .unwrap();
        a.send(
            b.node_id(),
            Bytes::from_static(b"keep"),
            DeliveryMethod::ReliableOrdered,
        )
        .await
        .unwrap();

        timeout(Duration::from_millis(100), async {
            loop {
                if handler.events().len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let events = handler.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, Bytes::from_static(b"keep"));
    }

    #[tokio::test]
    async fn link_condition_delays_delivery_without_blocking_send() {
        let net = Arc::new(SimNetwork::new(13));
        net.set_link_condition(
            node("a"),
            node("b"),
            LinkCondition::new(Duration::from_millis(200), Duration::ZERO, 0.0),
        );
        let a = net.transport(node("a"));
        let b = net.transport(node("b"));
        let handler = Arc::new(RecordingHandler::new());

        b.start(handler.clone()).await.unwrap();
        a.connect(b.node_id()).await.unwrap();

        let started = Instant::now();
        timeout(
            Duration::from_millis(50),
            a.send(
                b.node_id(),
                Bytes::from_static(b"slow"),
                DeliveryMethod::ReliableOrdered,
            ),
        )
        .await
        .unwrap()
        .unwrap();

        assert!(handler.events().is_empty());

        timeout(Duration::from_millis(500), async {
            loop {
                if !handler.events().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn same_link_deliveries_remain_ordered() {
        let net = Arc::new(SimNetwork::new(17));
        net.set_link_condition(
            node("a"),
            node("b"),
            LinkCondition::new(Duration::from_millis(80), Duration::ZERO, 0.0),
        );
        let a = net.transport(node("a"));
        let b = net.transport(node("b"));
        let handler = Arc::new(RecordingHandler::new());

        b.start(handler.clone()).await.unwrap();
        a.connect(b.node_id()).await.unwrap();

        a.send(
            b.node_id(),
            Bytes::from_static(b"first"),
            DeliveryMethod::ReliableOrdered,
        )
        .await
        .unwrap();
        net.set_link_condition(
            node("a"),
            node("b"),
            LinkCondition::new(Duration::ZERO, Duration::ZERO, 0.0),
        );
        a.send(
            b.node_id(),
            Bytes::from_static(b"second"),
            DeliveryMethod::ReliableOrdered,
        )
        .await
        .unwrap();

        timeout(Duration::from_millis(500), async {
            loop {
                if handler.events().len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let events = handler.events();
        assert_eq!(events[0].data, Bytes::from_static(b"first"));
        assert_eq!(events[1].data, Bytes::from_static(b"second"));
    }

    #[tokio::test]
    async fn burst_deliveries_are_pipelined_by_enqueue_time() {
        let net = Arc::new(SimNetwork::new(19));
        net.set_link_condition(
            node("a"),
            node("b"),
            LinkCondition::new(Duration::from_millis(50), Duration::from_millis(20), 0.0),
        );
        let a = net.transport(node("a"));
        let b = net.transport(node("b"));
        let handler = Arc::new(TimedRecordingHandler::new());

        b.start(handler.clone()).await.unwrap();
        a.connect(b.node_id()).await.unwrap();

        let started = Instant::now();
        for seq in 0..30_u8 {
            a.send(
                b.node_id(),
                Bytes::from(vec![seq]),
                DeliveryMethod::ReliableOrdered,
            )
            .await
            .unwrap();
        }

        timeout(Duration::from_millis(300), async {
            loop {
                if handler.events().len() == 30 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let events = handler.events();
        let first_elapsed = events.first().unwrap().1.duration_since(started);
        let last_elapsed = events.last().unwrap().1.duration_since(started);

        assert!(first_elapsed >= Duration::from_millis(50));
        assert!(
            last_elapsed <= Duration::from_millis(150),
            "burst delivery took {:?}",
            last_elapsed
        );
        for (seq, (event, _)) in events.iter().enumerate() {
            assert_eq!(event.data, Bytes::from(vec![seq as u8]));
        }
    }
}
