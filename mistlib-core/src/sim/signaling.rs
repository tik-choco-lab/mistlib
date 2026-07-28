use super::{duration_nanos_u64, LinkCondition};
use crate::error::{MistError, Result};
use crate::signaling::{MessageContent, Signaler, SignalingHandler};
use crate::types::NodeId;
use async_trait::async_trait;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub struct SimSignalingHub {
    handlers: RwLock<HashMap<NodeId, Arc<dyn SignalingHandler>>>,
    links: RwLock<HashMap<(NodeId, NodeId), LinkCondition>>,
    default_condition: RwLock<LinkCondition>,
    rng: Mutex<StdRng>,
}

impl SimSignalingHub {
    pub fn new(seed: u64) -> Self {
        Self {
            handlers: RwLock::new(HashMap::new()),
            links: RwLock::new(HashMap::new()),
            default_condition: RwLock::new(LinkCondition::default()),
            rng: Mutex::new(StdRng::seed_from_u64(seed)),
        }
    }

    pub fn join(
        self: &Arc<Self>,
        node_id: NodeId,
        handler: Arc<dyn SignalingHandler>,
    ) -> SimSignaler {
        self.handlers
            .write()
            .unwrap()
            .insert(node_id.clone(), handler);
        SimSignaler {
            node_id,
            hub: Arc::clone(self),
            closed: Mutex::new(false),
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

    pub fn registered_nodes(&self) -> Vec<NodeId> {
        self.handlers.read().unwrap().keys().cloned().collect()
    }

    fn handler_for(&self, node_id: &NodeId) -> Option<Arc<dyn SignalingHandler>> {
        self.handlers.read().unwrap().get(node_id).cloned()
    }

    fn remove_handler(&self, node_id: &NodeId) {
        self.handlers.write().unwrap().remove(node_id);
    }

    fn link_condition(&self, from: &NodeId, to: &NodeId) -> LinkCondition {
        self.links
            .read()
            .unwrap()
            .get(&(from.clone(), to.clone()))
            .copied()
            .unwrap_or_else(|| *self.default_condition.read().unwrap())
    }

    fn delivery_delay(&self, condition: LinkCondition) -> Duration {
        let mut rng = self.rng.lock().unwrap();
        let jitter = if condition.jitter.is_zero() {
            Duration::ZERO
        } else {
            Duration::from_nanos(rng.gen_range(0..=duration_nanos_u64(condition.jitter)))
        };
        condition.latency.saturating_add(jitter)
    }

    async fn send_from(
        self: &Arc<Self>,
        from: &NodeId,
        to: &NodeId,
        msg: MessageContent,
    ) -> Result<()> {
        if to.is_broadcast() {
            let recipients: Vec<_> = self
                .handlers
                .read()
                .unwrap()
                .iter()
                .filter(|(node_id, _)| *node_id != from)
                .map(|(node_id, handler)| (node_id.clone(), Arc::clone(handler)))
                .collect();

            for (recipient, handler) in recipients {
                self.spawn_delivery(from.clone(), recipient, msg.clone(), handler);
            }
            return Ok(());
        }

        let handler = self
            .handler_for(to)
            .ok_or_else(|| MistError::NodeNotFound(to.clone()))?;
        self.spawn_delivery(from.clone(), to.clone(), msg, handler);
        Ok(())
    }

    fn spawn_delivery(
        self: &Arc<Self>,
        from: NodeId,
        to: NodeId,
        msg: MessageContent,
        handler: Arc<dyn SignalingHandler>,
    ) {
        let delay = self.delivery_delay(self.link_condition(&from, &to));
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if hub
                .handler_for(&to)
                .as_ref()
                .is_some_and(|registered| Arc::ptr_eq(registered, &handler))
            {
                let _ = handler.handle_message(msg).await;
            }
        });
    }
}

pub struct SimSignaler {
    node_id: NodeId,
    hub: Arc<SimSignalingHub>,
    closed: Mutex<bool>,
}

impl SimSignaler {
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }
}

#[async_trait]
impl Signaler for SimSignaler {
    async fn send_signaling(&self, to: &NodeId, msg: MessageContent) -> Result<()> {
        if *self.closed.lock().unwrap() {
            return Err(MistError::Signaling(format!(
                "sim signaler {} is closed",
                self.node_id
            )));
        }
        self.hub.send_from(&self.node_id, to, msg).await
    }

    async fn close(&self) -> Result<()> {
        *self.closed.lock().unwrap() = true;
        self.hub.remove_handler(&self.node_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signaling::{SignalingData, SignalingType};
    use tokio::time::timeout;

    struct RecordingSignalingHandler {
        messages: Mutex<Vec<MessageContent>>,
    }

    impl RecordingSignalingHandler {
        fn new() -> Self {
            Self {
                messages: Mutex::new(Vec::new()),
            }
        }

        fn messages(&self) -> Vec<MessageContent> {
            self.messages.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SignalingHandler for RecordingSignalingHandler {
        async fn handle_message(&self, msg: MessageContent) -> Result<()> {
            self.messages.lock().unwrap().push(msg);
            Ok(())
        }
    }

    fn node(id: &str) -> NodeId {
        NodeId(id.to_string())
    }

    fn message(from: &str, to: NodeId) -> MessageContent {
        MessageContent::Data(SignalingData {
            sender_id: node(from),
            receiver_id: to,
            room_id: "room".to_string(),
            data: "payload".to_string(),
            signaling_type: SignalingType::Offer,
        })
    }

    async fn wait_for_len(handler: &RecordingSignalingHandler, expected: usize) {
        timeout(Duration::from_millis(100), async {
            loop {
                if handler.messages().len() == expected {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn direct_delivery_reaches_target_handler() {
        let hub = Arc::new(SimSignalingHub::new(31));
        let a_handler = Arc::new(RecordingSignalingHandler::new());
        let b_handler = Arc::new(RecordingSignalingHandler::new());
        let a = hub.join(node("a"), a_handler.clone());
        let b = hub.join(node("b"), b_handler.clone());

        a.send_signaling(b.node_id(), message("a", b.node_id().clone()))
            .await
            .unwrap();

        wait_for_len(&b_handler, 1).await;
        assert!(a_handler.messages().is_empty());
        let messages = b_handler.messages();
        let MessageContent::Data(data) = &messages[0] else {
            panic!("expected signaling data");
        };
        assert_eq!(data.sender_id, node("a"));
        assert_eq!(data.receiver_id, node("b"));
    }

    #[tokio::test]
    async fn broadcast_delivery_reaches_all_other_handlers() {
        let hub = Arc::new(SimSignalingHub::new(37));
        let a_handler = Arc::new(RecordingSignalingHandler::new());
        let b_handler = Arc::new(RecordingSignalingHandler::new());
        let c_handler = Arc::new(RecordingSignalingHandler::new());
        let a = hub.join(node("a"), a_handler.clone());
        hub.join(node("b"), b_handler.clone());
        hub.join(node("c"), c_handler.clone());

        a.send_signaling(&NodeId::broadcast(), message("a", NodeId::broadcast()))
            .await
            .unwrap();

        wait_for_len(&b_handler, 1).await;
        wait_for_len(&c_handler, 1).await;
        assert!(a_handler.messages().is_empty());
    }

    #[tokio::test]
    async fn close_unregisters_handler_and_blocks_delivery() {
        let hub = Arc::new(SimSignalingHub::new(41));
        let a_handler = Arc::new(RecordingSignalingHandler::new());
        let b_handler = Arc::new(RecordingSignalingHandler::new());
        let a = hub.join(node("a"), a_handler.clone());
        let b = hub.join(node("b"), b_handler.clone());

        b.close().await.unwrap();

        let err = a
            .send_signaling(b.node_id(), message("a", b.node_id().clone()))
            .await
            .unwrap_err();
        assert!(matches!(err, MistError::NodeNotFound(_)));
        assert!(b_handler.messages().is_empty());

        a.send_signaling(&NodeId::broadcast(), message("a", NodeId::broadcast()))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(b_handler.messages().is_empty());
    }
}
