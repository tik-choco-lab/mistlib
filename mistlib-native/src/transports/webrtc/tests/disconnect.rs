use super::*;
use async_trait::async_trait;
use mistlib_core::{
    error::Result as MistResult,
    signaling::{MessageContent, Signaler, SignalingHandler},
    transport::Transport,
    types::{ConnectionState, NodeId},
};
use std::sync::Arc;
use tokio::sync::mpsc;

struct LoopbackSignaler {
    tx: mpsc::UnboundedSender<MessageContent>,
}

#[async_trait]
impl Signaler for LoopbackSignaler {
    async fn send_signaling(&self, _to: &NodeId, msg: MessageContent) -> MistResult<()> {
        let _ = self.tx.send(msg);
        Ok(())
    }

    async fn close(&self) -> MistResult<()> {
        Ok(())
    }
}

fn make_connected_pair() -> (Arc<WebRtcTransport>, Arc<WebRtcTransport>, NodeId, NodeId) {
    let id_a = NodeId("peer-a".to_string());
    let id_b = NodeId("peer-b".to_string());

    let (tx_a_to_b, rx_a_to_b) = mpsc::unbounded_channel::<MessageContent>();
    let (tx_b_to_a, rx_b_to_a) = mpsc::unbounded_channel::<MessageContent>();

    let ta = Arc::new(WebRtcTransport::new(
        Arc::new(LoopbackSignaler { tx: tx_a_to_b }),
        id_a.clone(),
    ));
    let tb = Arc::new(WebRtcTransport::new(
        Arc::new(LoopbackSignaler { tx: tx_b_to_a }),
        id_b.clone(),
    ));

    // Route signaling messages: A→B and B→A
    let tb_route = tb.clone();
    tokio::spawn(async move {
        let mut rx = rx_a_to_b;
        while let Some(msg) = rx.recv().await {
            let _ = tb_route.handle_message(msg).await;
        }
    });
    let ta_route = ta.clone();
    tokio::spawn(async move {
        let mut rx = rx_b_to_a;
        while let Some(msg) = rx.recv().await {
            let _ = ta_route.handle_message(msg).await;
        }
    });

    (ta, tb, id_a, id_b)
}

async fn wait_for_state(
    transport: &WebRtcTransport,
    node: &NodeId,
    expected: ConnectionState,
    timeout_ms: u64,
) -> bool {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if transport.get_connection_state(node) == expected {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// WebRTC接続を確立してからA側がdisconnectしたとき、
/// B側のDataChannel on_closeコールバックが即座に発火して
/// connection_statesが更新されることを確認する。
/// ICEタイムアウト(~30s)を待たず2秒以内に検知できることが基準。
#[tokio::test]
async fn datachannel_close_notifies_remote_immediately() {
    let (ta, tb, id_a, id_b) = make_connected_pair();

    // A → B へ接続
    ta.connect(&id_b).await.expect("connect should not fail");

    // 両側がConnectedになるまで待つ (最大10秒)
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state. A→B: {:?}",
        ta.get_connection_state(&id_b),
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state. B→A: {:?}",
        tb.get_connection_state(&id_a),
    );

    // A側がgracefulにdisconnect → DataChannelのSCTP closeがBに伝わる
    ta.disconnect(&id_b)
        .await
        .expect("disconnect should not fail");
    let start = std::time::Instant::now();

    // B側が2秒以内に切断を検知すること (ICEタイムアウト=30sより十分短い)
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Disconnected, 2_000).await,
        "B did not detect A's disconnection within 2s (elapsed={:?}). \
         B→A state: {:?}. If ICE timeout is required, the on_close fix is not working.",
        start.elapsed(),
        tb.get_connection_state(&id_a),
    );

    tracing::info!("Disconnection detected by B in {:?}", start.elapsed());
}

/// on_closeが複数回呼ばれても on_disconnected_internal は1回だけ呼ばれることを確認。
/// peersからの削除が1回だけ成功することで重複を防いでいる。
#[tokio::test]
async fn datachannel_close_does_not_duplicate_cleanup() {
    let (ta, tb, id_a, id_b) = make_connected_pair();

    ta.connect(&id_b).await.expect("connect should not fail");

    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    ta.disconnect(&id_b)
        .await
        .expect("disconnect should not fail");

    // Bの状態がDisconnectedになった後、さらに時間を置いてもpeersが空のままであること
    let _ = wait_for_state(&tb, &id_a, ConnectionState::Disconnected, 2_000).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let peers_b = tb.peers.read().await;
    assert!(
        !peers_b.contains_key(&id_a),
        "B's peers map should not contain A after disconnect, but found a leaked entry"
    );
}
