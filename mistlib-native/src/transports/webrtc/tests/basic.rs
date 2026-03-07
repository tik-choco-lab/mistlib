use super::*;
use mistlib_core::types::{ConnectionState, NodeId};

#[tokio::test]
async fn disconnect_unknown_node_does_not_crash() {
    let t = make_transport();
    assert!(t.disconnect(&NodeId("unknown".to_string())).await.is_ok());
}

#[tokio::test]
async fn get_connection_state_unknown_is_disconnected() {
    let t = make_transport();
    assert_eq!(
        t.get_connection_state(&NodeId("nobody".to_string())),
        ConnectionState::Disconnected
    );
}

#[tokio::test]
async fn get_connected_nodes_empty_initially() {
    let t = make_transport();
    assert!(t.get_connected_nodes().is_empty());
}

#[tokio::test]
async fn connect_does_not_crash() {
    let t = make_transport();
    let result = t.connect(&NodeId("peer".to_string())).await;
    assert!(
        result.is_ok(),
        "connect should successfully initiate connection"
    );
}

#[tokio::test]
async fn connect_fails_cleanly_no_leaked_state() {
    let t = make_transport();
    let node = NodeId("peer".to_string());
    let _ = t.connect(&node).await;
    assert_eq!(
        t.get_connection_state(&node),
        ConnectionState::Connecting,
        "Node should enter Connecting state immediately after connect() is called"
    );
}

#[tokio::test]
async fn disconnect_after_connect_attempt_does_not_crash() {
    let t = make_transport();
    let node = NodeId("peer".to_string());
    let _ = t.connect(&node).await;
    assert!(t.disconnect(&node).await.is_ok());
}

#[tokio::test]
async fn repeated_disconnect_does_not_crash() {
    let t = make_transport();
    let node = NodeId("peer".to_string());
    assert!(t.disconnect(&node).await.is_ok());
    assert!(t.disconnect(&node).await.is_ok());
    assert!(t.disconnect(&node).await.is_ok());
}

#[tokio::test]
async fn send_to_unknown_node_does_not_crash() {
    use bytes::Bytes;
    use mistlib_core::types::DeliveryMethod;
    let t = make_transport();
    let result = t
        .send(
            &NodeId("nobody".to_string()),
            Bytes::from_static(b"hello"),
            DeliveryMethod::ReliableOrdered,
        )
        .await;
    assert!(
        result.is_err(),
        "sending to unknown peer should return an error"
    );
}

#[tokio::test]
async fn broadcast_with_no_connections_does_not_crash() {
    use bytes::Bytes;
    use mistlib_core::types::DeliveryMethod;
    let t = make_transport();
    assert!(t
        .broadcast(Bytes::from_static(b"hi"), DeliveryMethod::Unreliable)
        .await
        .is_ok());
}
