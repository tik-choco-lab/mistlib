//! Local send admission, byte accounting and ordering-safe lane selection.
use crate::action::{OverlayAction, SendLane};
use crate::config::StorageTransferConfig;
use crate::error::{MistError, Result};
use crate::overlay::OverlayEnvelope;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, Notify, Semaphore};

#[cfg(not(target_arch = "wasm32"))]
pub type SendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[cfg(target_arch = "wasm32")]
pub type SendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
pub type SendReceipt = oneshot::Receiver<Result<()>>;
#[cfg(not(target_arch = "wasm32"))]
pub type SendReservation = Box<dyn FnOnce(OverlayAction) -> Result<SendReceipt> + Send>;
#[cfg(target_arch = "wasm32")]
pub type SendReservation = Box<dyn FnOnce(OverlayAction) -> Result<SendReceipt>>;

/// Explicitly disabling the pipeline retains enqueue-only admission and legacy FIFO.
/// A custom byte budget also opts into bounded, awaitable storage responses.
pub fn pipeline_enabled(config: &StorageTransferConfig) -> bool {
    config.unsequenced_bulk
        || config.control_priority
        || config.bulk_channel
        || config.bulk_target_delay_ms > 0
        || config.bulk_queue_bytes != StorageTransferConfig::default().bulk_queue_bytes
}

/// Storage uses the public method only for validated unsequenced chunks.
/// A positive sequence stays on the ordered channel, including at relays.
pub fn storage_delivery(
    method: crate::types::DeliveryMethod,
    lane: SendLane,
    seq: u64,
    bulk_channel: bool,
) -> crate::types::DeliveryMethod {
    use crate::types::DeliveryMethod;
    if bulk_channel
        && method == DeliveryMethod::ReliableOrdered
        && lane == SendLane::Bulk
        && seq == 0
    {
        DeliveryMethod::ReliableUnordered
    } else {
        method
    }
}

pub fn closed_send() -> MistError {
    MistError::Network("send queue or channel closed".into())
}

/// Only failures known to precede submission may retry on another channel.
pub enum WriteFailure {
    Uncommitted(MistError),
    Failed(MistError),
}

pub async fn with_reliable_fallback<F, R>(
    primary: impl Future<Output = std::result::Result<(), WriteFailure>>,
    fallback: F,
) -> Result<()>
where
    F: FnOnce() -> R,
    R: Future<Output = Result<()>>,
{
    match primary.await {
        Ok(()) => Ok(()),
        Err(WriteFailure::Uncommitted(_)) => fallback().await,
        Err(WriteFailure::Failed(err)) => Err(err),
    }
}

/// Resizing changes new admission only. Outstanding permits remain charged,
/// including when a decrease leaves the queue in debt.
pub struct ByteBudget {
    ceiling: u32,
    shared: Arc<BudgetShared>,
}
struct BudgetShared {
    state: Mutex<BudgetState>,
    changed: Notify,
    turn: Arc<Semaphore>,
}
struct BudgetState {
    limit: u32,
    used: u32,
    closed: bool,
}
pub struct BytePermit {
    shared: Arc<BudgetShared>,
    bytes: u32,
}
impl BytePermit {
    fn shrink_to(&mut self, bytes: usize) {
        let bytes = bytes.min(self.bytes as usize) as u32;
        self.shared.state.lock().unwrap().used -= self.bytes - bytes;
        self.bytes = bytes;
        self.shared.changed.notify_waiters();
    }
}
impl Drop for BytePermit {
    fn drop(&mut self) {
        self.shrink_to(0);
    }
}
impl ByteBudget {
    pub fn new(limit: u32) -> Self {
        Self {
            ceiling: limit,
            shared: Arc::new(BudgetShared {
                state: Mutex::new(BudgetState {
                    limit,
                    used: 0,
                    closed: false,
                }),
                changed: Notify::new(),
                turn: Arc::new(Semaphore::new(1)),
            }),
        }
    }
    pub fn set_limit(&self, limit: u32) {
        self.shared.state.lock().unwrap().limit = limit.min(self.ceiling);
        self.shared.changed.notify_waiters();
    }
    pub fn is_closed(&self) -> bool {
        self.shared.state.lock().unwrap().closed
    }
    pub fn limit(&self) -> u32 {
        self.shared.state.lock().unwrap().limit
    }
    pub fn used(&self) -> u32 {
        self.shared.state.lock().unwrap().used
    }
    fn check(&self, bytes: usize) -> Result<u32> {
        if bytes > self.ceiling as usize {
            return Err(MistError::MessageTooLarge {
                size: bytes,
                limit: self.ceiling,
            });
        }
        Ok(bytes as u32)
    }
    fn acquire(
        shared: &Arc<BudgetShared>,
        bytes: u32,
        received: bool,
    ) -> Result<Option<BytePermit>> {
        let mut state = shared.state.lock().unwrap();
        if state.closed {
            return Err(closed_send());
        }
        // An indivisible received frame can exclusively occupy a smaller
        // adaptive budget. Never strand an already sequenced frame.
        if u64::from(state.used) + u64::from(bytes) <= u64::from(state.limit)
            || received && state.used == 0 && bytes > state.limit
        {
            state.used += bytes;
            Ok(Some(BytePermit {
                shared: shared.clone(),
                bytes,
            }))
        } else {
            Ok(None)
        }
    }
    fn reserve_owned(&self, bytes: u32, received: bool) -> SendFuture<'static, Result<BytePermit>> {
        let shared = self.shared.clone();
        Box::pin(async move {
            // FIFO admission, including cancellation; no state lock across await.
            let _turn = shared
                .turn
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| closed_send())?;
            loop {
                let changed = shared.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some(permit) = Self::acquire(&shared, bytes, received)? {
                    return Ok(permit);
                }
                changed.await;
            }
        })
    }
    pub async fn reserve(&self, bytes: usize) -> Result<BytePermit> {
        self.reserve_owned(self.check(bytes)?, false).await
    }
    pub async fn reserve_received(&self, bytes: usize) -> Result<BytePermit> {
        self.reserve_received_owned(bytes).await
    }
    pub fn reserve_received_owned(&self, bytes: usize) -> SendFuture<'static, Result<BytePermit>> {
        self.reserve_owned(bytes.min(self.ceiling as usize) as u32, true)
    }
    pub fn try_reserve(&self, bytes: usize) -> Result<BytePermit> {
        let bytes = self.check(bytes)?;
        let _turn = self.shared.turn.try_acquire().map_err(|_| closed_send())?;
        Self::acquire(&self.shared, bytes, false)?.ok_or_else(closed_send)
    }
    pub fn close(&self) {
        self.shared.state.lock().unwrap().closed = true;
        self.shared.turn.close();
        self.shared.changed.notify_waiters();
    }
}

pub fn shrink_permit(permit: &mut Option<BytePermit>, bytes: usize) {
    if let Some(permit) = permit {
        permit.shrink_to(bytes);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SendClass {
    pub lane: SendLane,
    pub unsequenced_bulk: bool,
}

impl SendClass {
    pub fn from_wire(data: &[u8]) -> Self {
        let Ok(envelope) = crate::overlay::wire::deserialize::<OverlayEnvelope>(data) else {
            return Self::default();
        };
        let lane = SendLane::classify(&envelope.content, envelope.to.is_broadcast());
        Self {
            lane,
            unsequenced_bulk: lane == SendLane::Bulk && envelope.seq == 0,
        }
    }
}

pub trait ScheduledSend {
    fn send_class(&self) -> SendClass;
    /// Public ReliableUnordered may use an independent writer even with flags off.
    fn reliable_unordered(&self) -> bool {
        false
    }
}

/// Public ReliableUnordered and, with priority enabled, validated seq-0 Bulk
/// can use the independent writer. Received positive-sequence Bulk remains a
/// reliable-ordered FIFO barrier, including at relays.
pub struct LaneQueue<T> {
    frames: VecDeque<T>,
    priority: bool,
    control_burst: usize,
}

impl<T: ScheduledSend> LaneQueue<T> {
    pub fn new(priority: bool) -> Self {
        Self {
            frames: VecDeque::new(),
            priority,
            control_burst: 0,
        }
    }
    pub fn push_back(&mut self, frame: T) {
        self.frames.push_back(frame);
    }
    pub fn len(&self) -> usize {
        self.frames.len()
    }
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
    pub fn pop(&mut self, ordered_ready: bool, bulk_ready: bool) -> Option<(T, bool)> {
        let first_frame = self.frames.front()?;
        let first = first_frame.send_class();
        let public_unordered = first_frame.reliable_unordered();
        if !self.priority && !public_unordered {
            return ordered_ready.then(|| (self.frames.pop_front().unwrap(), false));
        }
        if public_unordered || first.unsequenced_bulk {
            // Inspect the first FIFO barrier, never skip an ordered frame to
            // find a later control. Sixteen controls then one bulk is fair.
            if ordered_ready && (self.control_burst < 16 || !bulk_ready) {
                if let Some(index) = self.frames.iter().position(|f| {
                    !(f.reliable_unordered() || self.priority && f.send_class().unsequenced_bulk)
                }) {
                    if !self.priority
                        || public_unordered && first.lane != SendLane::Bulk
                        || self.frames[index].send_class().lane != SendLane::Bulk
                    {
                        self.control_burst += 1;
                        return Some((self.frames.remove(index).unwrap(), false));
                    }
                }
            }
            if bulk_ready {
                self.control_burst = 0;
                return Some((self.frames.pop_front().unwrap(), true));
            }
            None
        } else if ordered_ready {
            if first.lane == SendLane::Control {
                self.control_burst += 1;
            } else {
                self.control_burst = 0;
            }
            Some((self.frames.pop_front().unwrap(), false))
        } else {
            None
        }
    }
}

/// Per-peer queued plus active bulk bytes, independent of admission policy.
/// A frame owns its charge until submission completes or cancellation drops it.
#[derive(Default)]
pub struct QueueTelemetry {
    bytes: std::sync::atomic::AtomicU64,
    channel: std::sync::atomic::AtomicU8,
}
pub struct QueueCharge {
    telemetry: Arc<QueueTelemetry>,
    bytes: u64,
}
impl QueueTelemetry {
    pub fn charge(self: &Arc<Self>, bytes: usize, lane: SendLane) -> Option<QueueCharge> {
        if lane != SendLane::Bulk {
            return None;
        }
        let bytes = bytes as u64;
        self.bytes
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        Some(QueueCharge {
            telemetry: self.clone(),
            bytes,
        })
    }
    pub fn queued_bytes(&self) -> u64 {
        self.bytes.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn submitted_on(&self, unordered: bool) {
        self.channel.store(
            if unordered { 2 } else { 1 },
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    pub fn bulk_channel(&self) -> Option<&'static str> {
        match self.channel.load(std::sync::atomic::Ordering::Relaxed) {
            1 => Some("reliable"),
            2 => Some("reliable-unordered"),
            _ => None,
        }
    }
}
impl QueueCharge {
    pub fn submitted_on(&self, unordered: bool) {
        self.telemetry.submitted_on(unordered);
    }
}
impl Drop for QueueCharge {
    fn drop(&mut self) {
        self.telemetry
            .bytes
            .fetch_sub(self.bytes, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Frame(u64, SendClass);
    impl ScheduledSend for Frame {
        fn send_class(&self) -> SendClass {
            self.1
        }
    }
    fn bulk(id: u64, seq: u64) -> Frame {
        Frame(
            id,
            SendClass {
                lane: SendLane::Bulk,
                unsequenced_bulk: seq == 0,
            },
        )
    }
    fn control(id: u64) -> Frame {
        Frame(
            id,
            SendClass {
                lane: SendLane::Control,
                unsequenced_bulk: false,
            },
        )
    }

    #[test]
    fn queue_telemetry_retains_active_bytes_until_drop_and_records_actual_channel() {
        let telemetry = Arc::new(QueueTelemetry::default());
        let cid = crate::storage::cid::compute_cid(b"data", 0x55);
        let payload = crate::storage::protocol::build_have_chunk_message(&cid, 0, 2, b"data");
        let envelope = OverlayEnvelope::new(
            crate::types::NodeId("a".into()),
            crate::types::NodeId("b".into()),
            1,
            crate::signaling::MessageContent::Raw(bytes::Bytes::from(payload)),
        );
        let data = crate::overlay::wire::serialize(&envelope).unwrap();
        let charge = telemetry.charge(data.len(), SendLane::Bulk).unwrap();
        assert_eq!(telemetry.queued_bytes(), data.len() as u64);
        assert_eq!(telemetry.bulk_channel(), None);
        charge.submitted_on(true);
        assert_eq!(telemetry.bulk_channel(), Some("reliable-unordered"));
        charge.submitted_on(false);
        assert_eq!(telemetry.bulk_channel(), Some("reliable"));
        drop(charge);
        assert_eq!(telemetry.queued_bytes(), 0);
        assert!(telemetry.charge(7, SendLane::Control).is_none());
    }

    #[test]
    fn only_zero_sequence_bulk_yields_to_control_and_bursts_are_bounded() {
        let mut queue = LaneQueue::new(true);
        queue.push_back(bulk(0, 0));
        for id in 1..=32 {
            queue.push_back(control(id));
        }
        for id in 1..=16 {
            assert_eq!(queue.pop(true, true).unwrap().0 .0, id);
        }
        assert_eq!(queue.pop(true, true).unwrap().0 .0, 0);
        let mut queue = LaneQueue::new(true);
        queue.push_back(bulk(1, 1));
        queue.push_back(control(2));
        assert_eq!(queue.pop(true, true).unwrap().0 .0, 1);
        assert!(
            queue.pop(false, true).is_none(),
            "second sequenced write must wait"
        );
        assert_eq!(queue.pop(true, true).unwrap().0 .0, 2);
    }

    #[test]
    fn priority_off_scheduler_preserves_fifo_even_for_zero_bulk() {
        let mut queue = LaneQueue::new(false);
        queue.push_back(bulk(0, 0));
        queue.push_back(control(1));
        assert_eq!(queue.pop(true, true).unwrap().0 .0, 0);
        assert_eq!(queue.pop(true, true).unwrap().0 .0, 1);
    }

    #[tokio::test]
    async fn resizing_retains_debt_and_wakes_on_growth_without_dropping_permits() {
        let budget = Arc::new(ByteBudget::new(100));
        let active = budget.reserve(80).await.unwrap();
        budget.set_limit(50);
        assert_eq!(budget.used(), 80);
        let other = budget.clone();
        let waiting = tokio::spawn(async move { other.reserve(20).await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        budget.set_limit(100);
        let mut admitted = Some(waiting.await.unwrap().unwrap());
        shrink_permit(&mut admitted, 10);
        assert_eq!(budget.used(), 90);
        drop(admitted);
        drop(active);
        assert_eq!(budget.used(), 0);
        budget.set_limit(10);
        // A frame is indivisible, including previously sequenced relays.
        let oversized = budget.reserve_received(200).await.unwrap();
        assert_eq!(budget.used(), 100);
        assert!(budget.try_reserve(1).is_err());
        drop(oversized);
    }

    #[tokio::test(start_paused = true)]
    async fn byte_budget_suspends_until_active_write_releases_and_close_wakes_waiters() {
        let budget = Arc::new(ByteBudget::new(100));
        let active = budget.reserve(80).await.unwrap();
        let other = budget.clone();
        let producer = tokio::spawn(async move { other.reserve(30).await });
        tokio::task::yield_now().await;
        assert!(!producer.is_finished());
        drop(active);
        let permit = producer.await.unwrap().unwrap();
        let other = budget.clone();
        let waiting = tokio::spawn(async move { other.reserve(100).await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        budget.close();
        assert!(waiting.await.unwrap().is_err());
        drop(permit);
        assert!(budget.reserve(101).await.is_err());
    }
}
