//! Cumulative admission/write counters. Relaxed atomics never coordinate work;
//! snapshots may straddle a frame, and are never reset by get_stats.
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

macro_rules! counters {
    ($($field:ident),+ $(,)?) => {
        #[derive(Default)]
        pub struct StorageCounters { $(pub $field: AtomicU64),+ }
        #[derive(Debug, Default, Serialize)]
        #[serde(rename_all = "camelCase")]
        pub struct StorageSnapshot { $(pub $field: u64),+ }
        impl StorageCounters {
            pub fn snapshot(&self) -> StorageSnapshot {
                StorageSnapshot { $($field: self.$field.load(Ordering::Relaxed)),+ }
            }
        }
    };
}
counters!(
    accepted_chunks,
    accepted_bytes,
    duplicate_chunks,
    duplicate_bytes,
    rejected_chunks,
    blocks_completed,
    want_sent,
    query_sent,
    have_status_received,
    responder_chunks_submitted,
    responder_bytes_submitted,
    responder_write_errors
);

pub static STORAGE_COUNTERS: LazyLock<Arc<StorageCounters>> =
    LazyLock::new(|| Arc::new(StorageCounters::default()));

impl StorageCounters {
    pub fn accepted(&self, bytes: usize) {
        self.accepted_chunks.fetch_add(1, Ordering::Relaxed);
        self.accepted_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }
    pub fn duplicate(&self, bytes: usize) {
        self.duplicate_chunks.fetch_add(1, Ordering::Relaxed);
        self.duplicate_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }
    pub fn rejected(&self) {
        self.rejected_chunks.fetch_add(1, Ordering::Relaxed);
    }
    pub fn completed(&self) {
        self.blocks_completed.fetch_add(1, Ordering::Relaxed);
    }
}
