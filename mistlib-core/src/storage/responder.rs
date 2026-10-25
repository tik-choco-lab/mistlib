//! Admission before spawning responders or loading blocks. Pending work stores
//! descriptors only, with four active workers and 64 pending WANTs per peer.
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

pub enum WantAdmission<D> {
    Start(D),
    Queued,
    Dropped,
}

struct PeerWork<D> {
    active: usize,
    pending: VecDeque<D>,
}

pub struct WantQueue<K, D> {
    peers: HashMap<K, PeerWork<D>>,
}

impl<K, D> Default for WantQueue<K, D> {
    fn default() -> Self {
        Self {
            peers: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash, D> WantQueue<K, D> {
    pub fn enqueue(&mut self, key: K, descriptor: D) -> WantAdmission<D> {
        let work = self.peers.entry(key).or_insert_with(|| PeerWork {
            active: 0,
            pending: VecDeque::new(),
        });
        if work.active < 4 {
            work.active += 1;
            WantAdmission::Start(descriptor)
        } else if work.pending.len() < 64 {
            work.pending.push_back(descriptor);
            WantAdmission::Queued
        } else {
            WantAdmission::Dropped
        }
    }

    /// Called by an active worker after one block. No new task is spawned for
    /// queued requests, so suspended responders cannot grow without bound.
    pub fn next(&mut self, key: &K) -> Option<D> {
        let work = self.peers.get_mut(key)?;
        if let Some(next) = work.pending.pop_front() {
            return Some(next);
        }
        work.active -= 1;
        if work.active == 0 {
            self.peers.remove(key);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_active_workers_and_pending_descriptors_per_peer() {
        let mut queue = WantQueue::default();
        for i in 0..4 {
            assert!(matches!(queue.enqueue("a", i), WantAdmission::Start(_)));
        }
        for i in 4..68 {
            assert!(matches!(queue.enqueue("a", i), WantAdmission::Queued));
        }
        assert!(matches!(queue.enqueue("a", 68), WantAdmission::Dropped));
        assert!(matches!(queue.enqueue("b", 0), WantAdmission::Start(_)));
        for i in 4..68 {
            assert_eq!(queue.next(&"a"), Some(i));
        }
        for _ in 0..4 {
            assert_eq!(queue.next(&"a"), None);
        }
        assert!(!queue.peers.contains_key("a"));
    }
}

/// Shared provider loop. A failed local write ends the block immediately;
/// already committed frames remain owned by their transport workers.
pub async fn send_block(
    transport: &dyn crate::transport::Transport,
    peer: &crate::types::NodeId,
    cid: &str,
    data: &[u8],
    chunk_size: usize,
    total_chunks: u16,
    mut after_frame: impl FnMut(u16) -> crate::transport::send::SendFuture<'static, ()>,
) -> crate::error::Result<()> {
    for index in 0..total_chunks.max(1) {
        let frame = super::protocol::build_have_payload(cid, data, index, total_chunks, chunk_size);
        let result = transport
            .send_response(
                peer,
                bytes::Bytes::from(frame),
                crate::types::DeliveryMethod::ReliableOrdered,
            )
            .await;
        let counters = &super::telemetry::STORAGE_COUNTERS;
        if let Err(error) = result {
            counters
                .responder_write_errors
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Err(error);
        }
        let bytes = if total_chunks <= 1 {
            data.len()
        } else {
            data.len()
                .saturating_sub(index as usize * chunk_size)
                .min(chunk_size)
        };
        counters
            .responder_chunks_submitted
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        counters
            .responder_bytes_submitted
            .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
        after_frame(index).await;
    }
    Ok(())
}

#[cfg(test)]
mod send_tests {
    use super::*;
    use crate::error::{MistError, Result};
    use crate::transport::{NetworkEventHandler, Transport};
    use crate::types::{ConnectionState, DeliveryMethod, NodeId};
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct BrokenWriter(AtomicUsize);
    #[async_trait]
    impl Transport for BrokenWriter {
        async fn start(&self, _: Arc<dyn NetworkEventHandler>) -> Result<()> {
            Ok(())
        }
        async fn send(&self, _: &NodeId, _: Bytes, _: DeliveryMethod) -> Result<()> {
            panic!("must await write completion")
        }
        async fn send_wait(&self, _: &NodeId, _: Bytes, _: DeliveryMethod) -> Result<()> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 1 {
                Err(MistError::Network("channel closed".into()))
            } else {
                Ok(())
            }
        }
        async fn broadcast(&self, _: Bytes, _: DeliveryMethod) -> Result<()> {
            unreachable!()
        }
        fn get_connection_state(&self, _: &NodeId) -> ConnectionState {
            ConnectionState::Connected
        }
        async fn connect(&self, _: &NodeId) -> Result<()> {
            Ok(())
        }
        async fn disconnect(&self, _: &NodeId) -> Result<()> {
            Ok(())
        }
        fn get_connected_nodes(&self) -> Vec<NodeId> {
            vec![]
        }
    }
    #[tokio::test(start_paused = true)]
    async fn first_write_error_stops_the_remaining_block_frames() {
        let writer = BrokenWriter(AtomicUsize::new(0));
        let data = vec![0; 64 * 1024];
        let cid = crate::storage::cid::compute_cid(&data, 0x55);
        let result = send_block(
            &writer,
            &NodeId("peer".into()),
            &cid,
            &data,
            16 * 1024,
            4,
            |_| Box::pin(async {}),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(writer.0.load(Ordering::SeqCst), 2);
    }
}
