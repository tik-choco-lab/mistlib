use crate::action::OverlayAction;
use crate::overlay::OverlayEnvelope;
use crate::overlay::{ActionHandler, OverlayRouter};
use crate::signaling::{MessageContent, Signaler};
use crate::transport::{NetworkEventHandler, Transport};
use crate::types::{ConnectionState, DeliveryMethod, NodeId};
use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;

pub struct OverlayTransport {
    pub router: Arc<OverlayRouter>,
    pub action_handler: Arc<dyn ActionHandler>,
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Transport for OverlayTransport {
    async fn start(&self, _handler: Arc<dyn NetworkEventHandler>) -> crate::error::Result<()> {
        Ok(())
    }

    async fn send(
        &self,
        to: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> crate::error::Result<()> {
        self.router.dispatch_data(
            to,
            data,
            method,
            |_| Ok(()),
            |action| self.action_handler.try_dispatch_send(action),
        )
    }

    async fn send_response(
        &self,
        to: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> crate::error::Result<()> {
        if crate::transport::send::pipeline_enabled(&self.router.transfer) {
            self.send_wait(to, data, method).await
        } else {
            self.send(to, data, method).await
        }
    }

    async fn send_wait(
        &self,
        to: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> crate::error::Result<()> {
        let next_hop = self
            .router
            .routing_table
            .lock()
            .unwrap()
            .get_next_hop(to)
            .ok_or_else(|| crate::error::MistError::RouteNotFound(to.clone()))?;
        let content = MessageContent::Raw(data.clone());
        let lane = crate::action::SendLane::classify(&content, to.is_broadcast());
        // Max-width varints bound the actual envelope without consuming a seq.
        let mut preview = OverlayEnvelope::new(
            self.router.local_node_id.clone(),
            to.clone(),
            self.router.hop_count,
            content,
        )
        .with_seq(u64::MAX);
        preview.msg_id = u64::MAX;
        if lane == crate::action::SendLane::Bulk && self.router.transfer.unsequenced_bulk {
            preview.seq = 0;
        }
        let bound = crate::overlay::wire::serialize(&preview)?.len();
        let reservation = self
            .action_handler
            .reserve_send(&next_hop, bound, lane)
            .await?;
        let receipt = self.router.dispatch_data_via(
            to,
            Some(&next_hop),
            data,
            method,
            |_| Ok(()),
            reservation,
        )?;
        receipt
            .await
            .map_err(|_| crate::transport::send::closed_send())?
    }

    async fn broadcast(&self, data: Bytes, method: DeliveryMethod) -> crate::error::Result<()> {
        self.send(&NodeId::broadcast(), data, method).await
    }

    fn get_connection_state(&self, _node: &NodeId) -> ConnectionState {
        ConnectionState::Connected
    }

    fn is_reachable(&self, node: &NodeId) -> bool {
        self.router
            .routing_table
            .lock()
            .unwrap()
            .get_next_hop(node)
            .is_some()
    }

    async fn connect(&self, _node: &NodeId) -> crate::error::Result<()> {
        Ok(())
    }

    async fn disconnect(&self, _node: &NodeId) -> crate::error::Result<()> {
        Ok(())
    }

    fn get_connected_nodes(&self) -> Vec<NodeId> {
        let rt = self.router.routing_table.lock().unwrap();
        rt.connected_nodes.iter().cloned().collect()
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Signaler for OverlayTransport {
    async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> crate::error::Result<()> {
        let envelope = OverlayEnvelope::new(
            self.router.local_node_id.clone(),
            to.clone(),
            self.router.hop_count,
            msg,
        );
        self.router.remember_outgoing(&envelope);
        let data = crate::overlay::wire::serialize(&envelope)
            .map_err(|e| crate::error::MistError::Internal(e.to_string()))?;
        let next_hop = {
            let rt = self.router.routing_table.lock().unwrap();
            rt.get_next_hop(to)
        };
        let next_hop =
            next_hop.ok_or_else(|| crate::error::MistError::RouteNotFound(to.clone()))?;

        self.action_handler
            .handle_action(OverlayAction::SendMessage {
                to: next_hop,
                data: Bytes::from(data),
                method: DeliveryMethod::ReliableOrdered,
                lane: crate::action::SendLane::Control,
            });
        Ok(())
    }

    async fn close(&self) -> crate::error::Result<()> {
        Ok(())
    }
}

impl OverlayTransport {
    pub fn has_signaling_route(&self, to: &NodeId) -> bool {
        if to.is_server() || to.is_broadcast() {
            return false;
        }

        let rt = self.router.routing_table.lock().unwrap();
        rt.get_next_hop(to).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::overlay::node_store::NodeStore;
    use std::sync::{Barrier, Mutex};

    #[derive(Default)]
    struct RecordingHandler {
        sequences: Mutex<Vec<u64>>,
    }

    impl ActionHandler for RecordingHandler {
        fn handle_action(&self, action: OverlayAction) {
            self.try_dispatch_send(action).unwrap();
        }

        fn try_dispatch_send(&self, action: OverlayAction) -> crate::error::Result<()> {
            let OverlayAction::SendMessage { data, .. } = action else {
                panic!("expected send");
            };
            let envelope: OverlayEnvelope = crate::overlay::wire::deserialize(&data).unwrap();
            // Widen the old stamp/enqueue window without holding the queue lock.
            for _ in 0..16 {
                std::thread::yield_now();
            }
            self.sequences.lock().unwrap().push(envelope.seq);
            Ok(())
        }
    }

    #[test]
    fn concurrent_sends_enqueue_in_sequence_order() {
        const THREADS: usize = 8;
        const SENDS: usize = 200;
        let handler = Arc::new(RecordingHandler::default());
        let transport = OverlayTransport {
            router: Arc::new(OverlayRouter::new(
                &Config::default(),
                Arc::new(Mutex::new(NodeStore::new())),
                NodeId("sender".into()),
            )),
            action_handler: handler.clone(),
        };
        let barrier = Barrier::new(THREADS);
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let transport = &transport;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..SENDS {
                        futures::executor::block_on(transport.send(
                            &NodeId("peer".into()),
                            Bytes::from_static(b"payload"),
                            DeliveryMethod::ReliableOrdered,
                        ))
                        .unwrap();
                    }
                });
            }
        });
        let recorded = handler.sequences.lock().unwrap();
        assert_eq!(
            *recorded,
            (1..=(THREADS * SENDS) as u64).collect::<Vec<_>>()
        );
    }
}
