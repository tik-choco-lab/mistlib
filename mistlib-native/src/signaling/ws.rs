use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData};
use mistlib_core::stats::STATS;
use mistlib_core::types::NodeId;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
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
        tracing::info!("WebSocketSignaler: connecting to {}", self.url);
        let (ws_stream, _) = connect_async(&self.url)
            .await
            .map_err(|e| crate::error::MistError::Network(e.to_string()))?;
        let (mut write, mut read) = ws_stream.split();
        let (tx, mut rx) = mpsc::channel::<String>(1024);
        *self.sender.lock().await = Some(tx);
        let sender_handle = self.sender.clone();

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let bytes = msg.len() as u64;
                if let Err(e) = write.send(Message::Text(msg.into())).await {
                    tracing::error!("WebSocketSignaler: send failed: {}", e);
                    break;
                }
                STATS.add_send(bytes);
            }
        });

        tokio::spawn(async move {
            loop {
                let Some(msg) = read.next().await else { break };
                let parse_result = match msg {
                    Ok(Message::Text(text)) => {
                        STATS.add_receive(text.len() as u64);
                        serde_json::from_slice::<SignalingData>(text.as_bytes())
                    }
                    Ok(Message::Binary(bin)) => {
                        STATS.add_receive(bin.len() as u64);
                        serde_json::from_slice::<SignalingData>(&bin)
                    }
                    Ok(Message::Close(frame)) => {
                        tracing::info!("WebSocketSignaler: closed: {:?}", frame);
                        break;
                    }
                    Err(e) => {
                        tracing::error!("WebSocketSignaler: error: {}", e);
                        break;
                    }
                    _ => continue,
                };
                match parse_result {
                    Ok(data) => {
                        if incoming_tx.send(MessageContent::Data(data)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => tracing::warn!("WebSocketSignaler: decode failed: {}", e),
                }
            }
            *sender_handle.lock().await = None;
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
        let MessageContent::Data(data) = msg else {
            return Err(mistlib_core::error::MistError::Signaling(
                "WebSocketSignaler: unsupported message type".to_string(),
            ));
        };
        let data_str = serde_json::to_string(&data)
            .map_err(|e| mistlib_core::error::MistError::Serialization(e.to_string()))?;

        let sender = self.sender.lock().await;
        let tx = sender.as_ref().ok_or_else(|| {
            mistlib_core::error::MistError::Signaling(format!(
                "WebSocketSignaler: not connected to {}",
                self.url
            ))
        })?;
        tx.send(data_str).await.map_err(|e| {
            mistlib_core::error::MistError::Signaling(format!(
                "WebSocketSignaler: channel closed: {}",
                e
            ))
        })
    }

    async fn close(&self) -> mistlib_core::error::Result<()> {
        *self.sender.lock().await = None;
        Ok(())
    }
}
