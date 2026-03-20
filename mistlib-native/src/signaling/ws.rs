use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData};
use mistlib_core::types::NodeId;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

pub struct WebSocketSignaler {
    pub url: String,
    sender: Arc<Mutex<Option<mpsc::Sender<String>>>>,
}

impl WebSocketSignaler {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            sender: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn connect(
        &self,
        incoming_tx: mpsc::Sender<MessageContent>,
    ) -> crate::error::Result<()> {
        tracing::info!("WebSocketSignaler: Connecting to {}", self.url);
        let (ws_stream, _) = connect_async(&self.url).await.map_err(|e| {
            tracing::error!(
                "WebSocketSignaler: Failed to connect to {}: {}",
                self.url,
                e
            );
            crate::error::MistError::Network(e.to_string())
        })?;
        tracing::info!("WebSocketSignaler: Connected to {}", self.url);
        let (mut write, mut read) = ws_stream.split();
        let (tx, mut rx) = mpsc::channel::<String>(1024);
        {
            let mut sender = self.sender.lock().await;
            *sender = Some(tx);
        }

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Err(e) = write.send(Message::Text(msg.into())).await {
                    tracing::error!("{}", e);
                    break;
                }
            }
        });

        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Ok(data) = serde_json::from_str::<SignalingData>(&text) {
                            let _ = incoming_tx.try_send(MessageContent::Data(data));
                        }
                    }
                    Ok(Message::Binary(bin)) => {
                        if let Ok(data) = serde_json::from_slice::<SignalingData>(&bin) {
                            let _ = incoming_tx.try_send(MessageContent::Data(data));
                        }
                    }
                    Err(e) => {
                        tracing::error!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok(())
    }
}

#[async_trait]
impl Signaler for WebSocketSignaler {
    async fn send_signaling(
        &self,
        _to: &NodeId,
        msg: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        let sender = self.sender.lock().await;
        if let Some(tx) = sender.as_ref() {
            let data_str = match msg {
                MessageContent::Data(data) => serde_json::to_string(&data)
                    .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?,
                _ => {
                    return Err(mistlib_core::error::MistError::Internal(
                        "Unsupported message type".to_string(),
                    ))
                }
            };

            tx.try_send(data_str)
                .map_err(|e| mistlib_core::error::MistError::Internal(e.to_string()))?;
            Ok(())
        } else {
            tracing::error!(
                "WebSocketSignaler: Cannot send signaling, not connected to {}",
                self.url
            );
            Err(mistlib_core::error::MistError::Internal(
                "Not connected".to_string(),
            ))
        }
    }

    async fn close(&self) -> mistlib_core::error::Result<()> {
        let mut sender = self.sender.lock().await;
        *sender = None;
        Ok(())
    }
}
