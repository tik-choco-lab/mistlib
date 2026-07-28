use std::sync::Arc;

use mistlib_core::signaling::{MessageContent, Signaler};
use mistlib_core::transport::{NetworkEvent, Transport};
use tokio::sync::mpsc;

use crate::runtime::TokioRuntime;

use super::SessionCtx;

struct DummyActionHandler;
impl mistlib_core::overlay::ActionHandler for DummyActionHandler {
    fn handle_action(&self, _action: mistlib_core::action::OverlayAction) {}
}

struct NetworkEventHandlerAdapter(mpsc::UnboundedSender<NetworkEvent>);
impl mistlib_core::transport::NetworkEventHandler for NetworkEventHandlerAdapter {
    fn on_event(&self, event: NetworkEvent) {
        if self.0.send(event).is_err() {
            tracing::debug!("dropping network event: engine receiver closed");
            mistlib_core::stats::STATS.add_dropped_receive_event();
        }
    }
}

impl super::MistEngine {
    /// Starts one session's stack (bootstrap signaling, transports, overlay
    /// tick machinery, room announce), then pumps that session's network
    /// events until it's torn down. `ctx.cancel` (set by
    /// `leave_room`/`leave_room_id`) is what ends this loop and the
    /// signaling loop spawned alongside it -- each session runs entirely
    /// independently of any other active room.
    pub async fn run(&self, ctx: Arc<SessionCtx>) -> crate::error::Result<()> {
        let (network_tx, network_rx) = mpsc::unbounded_channel::<NetworkEvent>();
        let (sig_tx, sig_rx) = mpsc::channel::<MessageContent>(1024);

        ctx.reset_connection_loss_tracking();
        self.connect_bootstrap_signaler(&ctx, sig_tx).await?;
        self.start_transports(&ctx, network_tx).await?;
        self.start_overlay(&ctx).await;
        self.announce_to_room(&ctx).await?;
        self.spawn_signaling_loop(sig_rx, ctx.clone());
        self.process_network_events(network_rx, ctx).await;

        Ok(())
    }

    async fn connect_bootstrap_signaler(
        &self,
        ctx: &SessionCtx,
        sig_tx: mpsc::Sender<MessageContent>,
    ) -> crate::error::Result<()> {
        if let Some(signaler) = ctx.bootstrap_signaler.as_ref() {
            if let Some(webrtc) = ctx.webrtc_transport.as_ref() {
                let webrtc = webrtc.clone();
                signaler.set_on_session_reestablished(Arc::new(move || {
                    let webrtc = webrtc.clone();
                    tokio::spawn(async move {
                        if let Err(err) = webrtc.announce_to_room().await {
                            tracing::warn!(
                                "NativeEngine: signaling reannounce failed after reconnect: {:?}",
                                err
                            );
                        }
                    });
                }));
            }
            signaler.connect(sig_tx).await?;
        }
        Ok(())
    }

    async fn start_transports(
        &self,
        ctx: &SessionCtx,
        network_tx: mpsc::UnboundedSender<NetworkEvent>,
    ) -> crate::error::Result<()> {
        let adapter = Arc::new(NetworkEventHandlerAdapter(network_tx));
        if let Some(wt) = ctx.webrtc_transport.as_ref() {
            wt.start(adapter.clone()).await?;
        }
        ctx.transport.start(adapter).await?;
        Ok(())
    }

    async fn start_overlay(&self, ctx: &SessionCtx) {
        if let Some(ov) = ctx.overlay.as_ref() {
            let config = Arc::new(self.config.lock().unwrap().clone());
            let runtime = Arc::new(TokioRuntime::new(self.runtime.handle().clone()));
            ov.start(runtime, config, Arc::new(DummyActionHandler))
                .await;
        }
    }

    async fn announce_to_room(&self, ctx: &SessionCtx) -> crate::error::Result<()> {
        if let Some(wt) = ctx.webrtc_transport.as_ref() {
            wt.announce_to_room().await?;
        }
        Ok(())
    }

    pub(super) fn spawn_signaling_loop(
        &self,
        mut sig_rx: mpsc::Receiver<MessageContent>,
        ctx: Arc<SessionCtx>,
    ) {
        let handler = ctx.ws_signaling_handler.clone();
        let cancel = ctx.cancel.clone();

        self.runtime.handle().spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = sig_rx.recv() => {
                        let Some(msg) = msg else { break };
                        if let MessageContent::Data(ref d) = msg {
                            ctx.ensure_node_registered(&d.sender_id);
                        }
                        if let Err(err) = handler.handle_message(msg).await {
                            tracing::warn!("NativeEngine: ws signaling handler failed: {:?}", err);
                        }
                    }
                }
            }
        });
    }
}
