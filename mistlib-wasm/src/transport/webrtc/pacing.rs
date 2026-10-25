use mistlib_core::config::StorageTransferConfig;
use mistlib_core::transport::bulk_pacing::BulkLink;
use mistlib_core::transport::send::ByteBudget;
use mistlib_core::types::DeliveryMethod;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use web_sys::RtcDataChannel;

/// Browser observations are synchronous; only a paced bulk write waits.
/// The permanent low handler remains installed while thresholds change.
pub struct WasmPacing {
    link: Mutex<BulkLink>,
    pub budget: Arc<ByteBudget>,
    started: web_time::Instant,
    requested: Mutex<Option<RtcDataChannel>>,
}
impl WasmPacing {
    pub fn new(config: &StorageTransferConfig) -> Option<Arc<Self>> {
        if config.bulk_target_delay_ms == 0 {
            return None;
        }
        let link = BulkLink::new(config.bulk_target_delay_ms, false);
        let budget = Arc::new(ByteBudget::new(config.bulk_queue_bytes));
        budget.set_limit(link.policy.budget());
        Some(Arc::new(Self {
            link: Mutex::new(link),
            budget,
            started: web_time::Instant::now(),
            requested: Mutex::new(None),
        }))
    }
    fn channel(dc: &RtcDataChannel) -> u64 {
        u64::from(dc.label() == "reliable-unordered")
    }
    pub fn ready(&self, dc: &RtcDataChannel, bytes: usize) -> bool {
        *self.requested.lock().unwrap() = Some(dc.clone());
        let mut link = self.link.lock().unwrap();
        link.policy.note_frame(bytes);
        // A caller at the write gate is backlogged even for a direct send
        // that did not take the responder's admission permit.
        link.observe(
            Self::channel(dc),
            self.started.elapsed().as_millis() as u64,
            u64::from(dc.buffered_amount()),
            true,
        );
        let high = link.policy.budget();
        dc.set_buffered_amount_low_threshold(high / 2);
        self.budget.set_limit(high);
        link.can_submit(Self::channel(dc), bytes)
    }
    pub fn spawn_observer(
        self: &Arc<Self>,
        channels: Arc<RwLock<HashMap<DeliveryMethod, RtcDataChannel>>>,
        bulk: Arc<RwLock<Option<RtcDataChannel>>>,
    ) {
        let weak = Arc::downgrade(self);
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                gloo_timers::future::TimeoutFuture::new(
                    mistlib_core::transport::bulk_pacing::SAMPLE_INTERVAL_MS as u32,
                )
                .await;
                let Some(this) = weak.upgrade() else {
                    break;
                };
                if this.budget.is_closed() {
                    break;
                }
                let requested = this
                    .requested
                    .lock()
                    .unwrap()
                    .clone()
                    .filter(|dc| dc.ready_state() == web_sys::RtcDataChannelState::Open);
                let dc = requested
                    .or_else(|| {
                        bulk.read()
                            .unwrap()
                            .clone()
                            .filter(|dc| dc.ready_state() == web_sys::RtcDataChannelState::Open)
                    })
                    .or_else(|| {
                        channels
                            .read()
                            .unwrap()
                            .get(&DeliveryMethod::ReliableOrdered)
                            .cloned()
                    });
                let Some(dc) = dc else {
                    continue;
                };
                let mut link = this.link.lock().unwrap();
                link.observe(
                    Self::channel(&dc),
                    this.started.elapsed().as_millis() as u64,
                    u64::from(dc.buffered_amount()),
                    this.budget.used() > 0,
                );
                let high = link.policy.budget();
                dc.set_buffered_amount_low_threshold(high / 2);
                this.budget.set_limit(high);
            }
        });
    }
    pub fn submitted(&self, dc: &RtcDataChannel, bytes: usize, bulk: bool) {
        self.link
            .lock()
            .unwrap()
            .submitted(Self::channel(dc), bytes, bulk);
    }
}
