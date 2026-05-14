use super::{EngineState, MistEngine, RunningContext};
use crate::signaling::MessageContent;
use crate::transport::{NetworkEvent, NetworkEventHandler};
use std::collections::HashSet;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

struct NetworkEventSender(mpsc::UnboundedSender<NetworkEvent>);

impl NetworkEventHandler for NetworkEventSender {
    fn on_event(&self, event: NetworkEvent) {
        let _ = self.0.send(event);
    }
}

impl MistEngine {
    pub fn leave_room(&self) {
        self.run_generation.fetch_add(1, AtomicOrdering::Relaxed);

        let (transport_opt, network_transport_opt, websocket_signaler_opt, to_disconnect) = {
            let state = self.state.lock().expect("state lock poisoned");
            if let EngineState::Running(ctx) = &*state {
                let mut nodes: HashSet<_> =
                    ctx.transport.get_connected_nodes().into_iter().collect();
                if let Some(nt) = &ctx.network_transport {
                    nodes.extend(nt.get_connected_nodes());
                }
                (
                    Some(ctx.transport.clone()),
                    ctx.network_transport.clone(),
                    ctx.websocket_signaler.clone(),
                    nodes,
                )
            } else {
                (None, None, None, HashSet::new())
            }
        };

        if let Some(transport) = transport_opt {
            self.runtime.spawn(Box::pin(async move {
                if let Some(ws) = websocket_signaler_opt {
                    if let Err(err) = ws.close().await {
                        tracing::warn!("leave_room: websocket close failed: {:?}", err);
                    }
                }
                for node in to_disconnect {
                    if let Err(e) = transport.disconnect(&node).await {
                        tracing::warn!("leave_room: disconnect failed for {}: {:?}", node.0, e);
                    }
                    if let Some(nt) = &network_transport_opt {
                        if let Err(e) = nt.disconnect(&node).await {
                            tracing::warn!(
                                "leave_room: network disconnect failed for {}: {:?}",
                                node.0,
                                e
                            );
                        }
                    }
                }
            }));
        }

        *self.state.lock().expect("state lock poisoned") = EngineState::Idle;

        let mut store = self.node_store.lock().expect("node_store lock poisoned");
        store.nodes.clear();
        store.last_updated.clear();
        self.aoi_nodes
            .lock()
            .expect("aoi_nodes lock poisoned")
            .clear();
    }

    pub async fn run(
        self: Arc<Self>,
        ctx: RunningContext,
        mut sig_rx: mpsc::UnboundedReceiver<MessageContent>,
    ) -> Result<(), String> {
        let run_generation = self
            .run_generation
            .fetch_add(1, AtomicOrdering::Relaxed)
            .wrapping_add(1);
        let (tx, mut rx) = mpsc::unbounded_channel::<NetworkEvent>();

        let ctx_arc = Arc::new(ctx);
        *self.state.lock().expect("state lock poisoned") = EngineState::Running(ctx_arc.clone());

        if let Some(nt) = &ctx_arc.network_transport {
            nt.start(Arc::new(NetworkEventSender(tx.clone())))
                .await
                .map_err(|e| e.to_string())?;
        }
        ctx_arc
            .transport
            .start(Arc::new(NetworkEventSender(tx)))
            .await
            .map_err(|e| e.to_string())?;

        let ctx_sig = ctx_arc.clone();
        let self_sig = self.clone();
        self.runtime.spawn(Box::pin(async move {
            loop {
                if self_sig.run_generation.load(AtomicOrdering::Relaxed) != run_generation {
                    break;
                }
                let msg = match sig_rx.try_recv() {
                    Ok(msg) => msg,
                    Err(TryRecvError::Empty) => {
                        self_sig
                            .runtime
                            .sleep(web_time::Duration::from_millis(100))
                            .await;
                        continue;
                    }
                    Err(TryRecvError::Disconnected) => break,
                };
                if let Err(err) = ctx_sig.signaling_handler.handle_message(msg).await {
                    tracing::warn!("MistEngine: signaling handler failed: {:?}", err);
                }
            }
        }));

        let self_net = self.clone();
        self.runtime.spawn(Box::pin(async move {
            loop {
                if self_net.run_generation.load(AtomicOrdering::Relaxed) != run_generation {
                    break;
                }
                let event = match rx.try_recv() {
                    Ok(event) => event,
                    Err(TryRecvError::Empty) => {
                        self_net
                            .runtime
                            .sleep(web_time::Duration::from_millis(100))
                            .await;
                        continue;
                    }
                    Err(TryRecvError::Disconnected) => break,
                };
                self_net.process_network_event(event).await;
            }
        }));

        let self_tick = self.clone();
        self.runtime.spawn(Box::pin(async move {
            loop {
                self_tick
                    .runtime
                    .sleep(web_time::Duration::from_millis(1000))
                    .await;
                if self_tick.run_generation.load(AtomicOrdering::Relaxed) != run_generation {
                    break;
                }
                self_tick.tick().await;
            }
        }));

        Ok(())
    }
}
