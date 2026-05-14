use std::sync::Arc;

use mistlib_core::action::OverlayAction;
use mistlib_core::overlay::node_store::NodeStore;
use mistlib_core::overlay::ActionHandler;
use mistlib_core::signaling::MessageContent;
use mistlib_core::transport::{NetworkEvent, Transport};
use mistlib_core::types::NodeId;
use std::sync::Mutex as StdMutex;
use tokio::sync::mpsc;

use crate::runtime::TokioRuntime;

use super::{EngineState, RunningContext};

struct DummyActionHandler;
impl ActionHandler for DummyActionHandler {
    fn handle_action(&self, _action: OverlayAction) {}
}

struct NetworkEventHandlerAdapter(mpsc::Sender<NetworkEvent>);
impl mistlib_core::transport::NetworkEventHandler for NetworkEventHandlerAdapter {
    fn on_event(&self, event: NetworkEvent) {
        let _ = self.0.try_send(event);
    }
}

impl super::MistEngine {
    pub async fn run(&self, ctx: Arc<RunningContext>) -> crate::error::Result<()> {
        let (network_tx, network_rx) = mpsc::channel::<NetworkEvent>(8192);
        let (sig_tx, sig_rx) = mpsc::channel::<MessageContent>(1024);

        self.transition_to_running(ctx.clone()).await;
        self.connect_signaling_server(&ctx, sig_tx).await?;
        self.start_transports(&ctx, network_tx).await?;
        self.start_overlay(&ctx).await;
        self.announce_to_room(&ctx).await?;
        self.spawn_signaling_loop(sig_rx, ctx.clone());
        self.process_network_events(network_rx).await;

        Ok(())
    }

    async fn transition_to_running(&self, ctx: Arc<RunningContext>) {
        let mut state_lock = self.state.write().await;
        *state_lock = EngineState::Running(ctx);
        self.reset_connection_loss_tracking();
    }

    async fn connect_signaling_server(
        &self,
        ctx: &RunningContext,
        sig_tx: mpsc::Sender<MessageContent>,
    ) -> crate::error::Result<()> {
        if let Some(ws) = ctx.websocket_signaler.as_ref() {
            ws.connect(sig_tx).await?;
        }
        Ok(())
    }

    async fn start_transports(
        &self,
        ctx: &RunningContext,
        network_tx: mpsc::Sender<NetworkEvent>,
    ) -> crate::error::Result<()> {
        let adapter = Arc::new(NetworkEventHandlerAdapter(network_tx));
        if let Some(wt) = ctx.webrtc_transport.as_ref() {
            wt.start(adapter.clone()).await?;
        }
        ctx.transport.start(adapter).await?;
        Ok(())
    }

    async fn start_overlay(&self, ctx: &RunningContext) {
        if let Some(ov) = ctx.overlay.as_ref() {
            let config = Arc::new(self.config.lock().unwrap().clone());
            let runtime = Arc::new(TokioRuntime::new(self.runtime.handle().clone()));
            ov.start(runtime, config, Arc::new(DummyActionHandler))
                .await;
        }
    }

    async fn announce_to_room(&self, ctx: &RunningContext) -> crate::error::Result<()> {
        if let Some(wt) = ctx.webrtc_transport.as_ref() {
            wt.announce_to_room().await?;
        }
        Ok(())
    }

    pub(super) fn spawn_signaling_loop(
        &self,
        mut sig_rx: mpsc::Receiver<MessageContent>,
        ctx: Arc<RunningContext>,
    ) {
        let handler = ctx.ws_signaling_handler.clone();
        let node_store = self.node_store.clone();

        self.runtime.handle().spawn(async move {
            while let Some(msg) = sig_rx.recv().await {
                if let MessageContent::Data(ref d) = msg {
                    register_node_if_new(&node_store, &d.sender_id);
                }
                if let Err(err) = handler.handle_message(msg).await {
                    tracing::warn!("NativeEngine: ws signaling handler failed: {:?}", err);
                }
            }
        });
    }
}

fn register_node_if_new(node_store: &Arc<StdMutex<NodeStore>>, node_id: &NodeId) {
    if node_id.is_server() {
        return;
    }
    let mut store = node_store.lock().unwrap();
    if !store.nodes.contains_key(node_id) {
        store.update_node_position(
            node_id.clone(),
            mistlib_core::overlay::dnve3::Vector3::zero(),
        );
    }
}
