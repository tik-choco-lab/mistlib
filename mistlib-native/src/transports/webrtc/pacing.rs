use mistlib_core::config::StorageTransferConfig;
use mistlib_core::stats::STATS;
use mistlib_core::transport::bulk_pacing::{BulkLink, SAMPLE_INTERVAL_MS};
use mistlib_core::transport::send::{closed_send, ByteBudget};
use mistlib_core::types::{DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::{data_channel_state::RTCDataChannelState, RTCDataChannel};

/// Owned by one PC. Telemetry is independent of the send arbiter: webrtc-rs
/// may hold its internal DC mutex across a blocked send/buffer query.
pub(super) struct NativePacing {
    state: Mutex<Observation>,
    requested: Mutex<Option<Arc<RTCDataChannel>>>,
    pub budget: Arc<ByteBudget>,
}
struct Observation {
    link: BulkLink,
    buffered: Option<u64>,
    version: u64,
    submitting: u32,
}
impl NativePacing {
    pub fn new(config: &StorageTransferConfig) -> Option<Arc<Self>> {
        if config.bulk_target_delay_ms == 0 {
            return None;
        }
        let link = BulkLink::new(config.bulk_target_delay_ms, true);
        let budget = Arc::new(ByteBudget::new(config.bulk_queue_bytes));
        budget.set_limit(link.policy.budget());
        Some(Arc::new(Self {
            state: Mutex::new(Observation {
                link,
                buffered: None,
                version: 0,
                submitting: 0,
            }),
            requested: Mutex::new(None),
            budget,
        }))
    }
    /// Latest consistent observer sample; stats never wait on a DC query.
    pub fn snapshot(&self) -> (Option<u64>, Option<f64>) {
        let state = self.state.lock().unwrap();
        (state.buffered, state.link.policy.rate_bytes_per_second())
    }
    pub fn begin_submission(&self) {
        let mut state = self.state.lock().unwrap();
        state.version = state.version.wrapping_add(1);
        state.submitting += 1;
    }
    pub fn finish_submission(&self, dc: &RTCDataChannel, bytes: usize, bulk: bool, accepted: bool) {
        let mut state = self.state.lock().unwrap();
        state.version = state.version.wrapping_add(1);
        state.submitting -= 1;
        if accepted {
            state.link.submitted(u64::from(dc.id()), bytes, bulk);
        }
    }
    pub async fn wait(
        &self,
        dc: &Arc<RTCDataChannel>,
        bytes: usize,
        cancel: &CancellationToken,
    ) -> mistlib_core::error::Result<()> {
        *self.requested.lock().unwrap() = Some(dc.clone());
        loop {
            if dc.ready_state() != RTCDataChannelState::Open {
                return Err(closed_send());
            }
            {
                let mut state = self.state.lock().unwrap();
                let link = &mut state.link;
                link.policy.note_frame(bytes);
                self.budget.set_limit(link.policy.budget());
                if link.can_submit(u64::from(dc.id()), bytes) {
                    return Ok(());
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err(closed_send()),
                _ = tokio::time::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS)) => {},
            }
        }
    }
    pub fn spawn_observer(
        self: &Arc<Self>,
        node: NodeId,
        channels: Arc<RwLock<HashMap<DeliveryMethod, Arc<RTCDataChannel>>>>,
        unordered_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
        cancel: CancellationToken,
    ) {
        let this = self.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS)) => {},
                }
                // One outstanding query at most. A canceled PC drops it.
                let requested = this
                    .requested
                    .lock()
                    .unwrap()
                    .clone()
                    .filter(|dc| dc.ready_state() == RTCDataChannelState::Open);
                let dc = requested.or(unordered_channel
                    .read()
                    .await
                    .clone()
                    .filter(|dc| dc.ready_state() == RTCDataChannelState::Open));
                let dc = match dc {
                    Some(dc) => Some(dc),
                    None => channels
                        .read()
                        .await
                        .get(&DeliveryMethod::ReliableOrdered)
                        .cloned(),
                };
                let Some(dc) = dc else {
                    continue;
                };
                let version = {
                    let state = this.state.lock().unwrap();
                    if state.submitting != 0 {
                        continue;
                    }
                    state.version
                };
                let buffered = tokio::select! {
                    _ = cancel.cancelled() => break,
                    buffered = dc.buffered_amount() => buffered,
                };
                let mut state = this.state.lock().unwrap();
                // Pair a buffer snapshot with exactly the accepted-byte
                // counter at that instant, even if dc.send held the DC mutex.
                if state.version != version || state.submitting != 0 {
                    continue;
                }
                state.buffered = Some(buffered as u64);
                let link = &mut state.link;
                if let Some(rtt) = STATS.rtt_millis.lock().unwrap().get(&node).copied() {
                    link.policy.note_rtt(f64::from(rtt));
                }
                link.observe(
                    u64::from(dc.id()),
                    started.elapsed().as_millis() as u64,
                    buffered as u64,
                    this.budget.used() > 0,
                );
                this.budget.set_limit(link.policy.budget());
            }
            this.budget.close();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn registered_adaptive_budget_survives_old_generation_lookups() {
        let t = super::super::tests::make_transport();
        let config = StorageTransferConfig {
            bulk_target_delay_ms: 100,
            ..Default::default()
        };
        t.set_storage_transfer(config.clone());
        let node = NodeId("peer".into());
        let (old_tx, _old_rx) = tokio::sync::mpsc::channel(1);
        let old = t.bulk_budget(&node, &old_tx).unwrap();
        let (new_tx, _new_rx) = tokio::sync::mpsc::channel(1);
        let pacing = NativePacing::new(&config).unwrap();
        t.bulk_budgets
            .lock()
            .unwrap()
            .entry(node.clone())
            .or_default()
            .push((new_tx.downgrade(), pacing.budget.clone()));
        assert!(Arc::ptr_eq(&old, &t.bulk_budget(&node, &old_tx).unwrap()));
        assert!(Arc::ptr_eq(
            &pacing.budget,
            &t.bulk_budget(&node, &new_tx).unwrap()
        ));
        assert!(!pacing.budget.is_closed());
    }
    #[tokio::test]
    async fn adaptive_queue_debt_waits_and_generation_has_fresh_budget() {
        let config = StorageTransferConfig {
            bulk_target_delay_ms: 100,
            ..Default::default()
        };
        let pacing = NativePacing::new(&config).unwrap();
        let permit = pacing.budget.reserve(60000).await.unwrap();
        pacing.budget.set_limit(34000);
        assert!(pacing.budget.try_reserve(1000).is_err());
        let fresh = NativePacing::new(&config).unwrap();
        assert!(fresh.budget.try_reserve(60000).is_ok());
        drop(permit);
        assert!(pacing.budget.try_reserve(17000).is_ok());
        assert!(NativePacing::new(&StorageTransferConfig::default()).is_some());
        let disabled = StorageTransferConfig {
            bulk_target_delay_ms: 0,
            ..Default::default()
        };
        assert!(NativePacing::new(&disabled).is_none());
    }
}
