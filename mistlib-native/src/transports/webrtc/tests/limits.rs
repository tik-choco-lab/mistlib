use super::*;
use mistlib_core::types::{ConnectionState, NodeId};

#[tokio::test]
async fn connect_at_max_is_silently_skipped() {
    let t = make_transport();
    t.set_max_connections(0);
    let result = t.connect(&NodeId("peer".to_string())).await;
    assert!(
        result.is_ok(),
        "connect at max=0 must return Ok (not crash)"
    );
    assert_eq!(
        t.get_connection_state(&NodeId("peer".to_string())),
        ConnectionState::Disconnected,
        "Node must not be added when max=0"
    );
}

#[tokio::test]
async fn many_concurrent_connects_do_not_crash() {
    use std::sync::Arc as StdArc;
    let t = StdArc::new(make_transport());
    t.set_max_connections(3);

    let mut handles = Vec::new();
    for i in 0..20u32 {
        let tc = StdArc::clone(&t);
        handles.push(tokio::spawn(async move {
            let _ = tc.connect(&NodeId(format!("peer-{i}"))).await;
        }));
    }
    for h in handles {
        assert!(h.await.is_ok(), "spawned task must not panic");
    }

    let active = t.get_active_connection_states();
    assert!(
        active.len() <= 3,
        "active connections ({}) must not exceed max (3)",
        active.len()
    );
}

#[tokio::test]
async fn active_connection_states_count_never_exceeds_max() {
    use std::sync::Arc as StdArc;
    const MAX: u32 = 5;
    let t = StdArc::new(make_transport());
    t.set_max_connections(MAX);

    let mut handles = Vec::new();
    for i in 0..50u32 {
        let tc = StdArc::clone(&t);
        handles.push(tokio::spawn(async move {
            let _ = tc.connect(&NodeId(format!("c-{i}"))).await;
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    let count = t.get_active_connection_states().len();
    assert!(
        count <= MAX as usize,
        "active_connection_states() ({count}) must not exceed max ({MAX})"
    );
}

#[tokio::test]
async fn connect_returns_ok_even_when_at_capacity() {
    let t = make_transport();
    t.set_max_connections(0);
    for i in 0..10u32 {
        let r = t.connect(&NodeId(format!("p{i}"))).await;
        assert!(r.is_ok(), "connect must always return Ok, got Err at i={i}");
    }
}

#[tokio::test]
async fn get_connected_nodes_does_not_crash_under_concurrent_access() {
    use std::sync::Arc as StdArc;
    let t = StdArc::new(make_transport());
    t.set_max_connections(5);

    let mut handles = Vec::new();
    for i in 0..10u32 {
        let tc = StdArc::clone(&t);
        handles.push(tokio::spawn(async move {
            let _ = tc.connect(&NodeId(format!("p-{i}"))).await;
            let _ = tc.get_connected_nodes();
            let _ = tc.get_active_connection_states();
        }));
    }
    for h in handles {
        assert!(h.await.is_ok());
    }
}

#[tokio::test]
async fn connect_sequential_respects_max_limit() {
    let t = make_transport();
    t.set_max_connections(2);

    let n1 = NodeId("peer_1".to_string());
    let n2 = NodeId("peer_2".to_string());
    let n3 = NodeId("peer_3".to_string());

    let _ = t.connect(&n1).await;
    let _ = t.connect(&n2).await;

    assert_eq!(t.get_connection_state(&n1), ConnectionState::Connecting);
    assert_eq!(t.get_connection_state(&n2), ConnectionState::Connecting);

    let result = t.connect(&n3).await;
    assert!(
        result.is_ok(),
        "connect at capacity should safely return Ok"
    );

    assert_eq!(
        t.get_connection_state(&n3),
        ConnectionState::Disconnected,
        "The 3rd connection attempt must be ignored and remain Disconnected"
    );

    let active = t.get_active_connection_states();
    assert_eq!(
        active.len(),
        2,
        "Total active connections must be exactly 2"
    );
}

#[tokio::test]
async fn connect_stress_limit_with_wait() {
    use web_time::Duration;
    use tokio::time::sleep;

    let t = make_transport();
    const MAX: u32 = 30;
    t.set_max_connections(MAX);

    for i in 0..50 {
        let node = NodeId(format!("stress-node-{}", i));
        let _ = t.connect(&node).await;
    }

    sleep(Duration::from_millis(100)).await;

    let active_states = t.get_active_connection_states();
    assert_eq!(
        active_states.len(),
        MAX as usize,
        "Active connections must be strictly limited to {} even after waiting",
        MAX
    );
}

#[tokio::test]
async fn disconnect_cleans_up_peers_and_states() {
    use web_time::Duration;
    use tokio::time::sleep;

    let t = make_transport();
    let node = NodeId("peer_to_kill".to_string());

    let _ = t.connect(&node).await;

    assert_eq!(t.get_connection_state(&node), ConnectionState::Connecting);

    let _ = t.disconnect(&node).await;

    sleep(Duration::from_millis(50)).await;

    assert_eq!(t.get_connection_state(&node), ConnectionState::Disconnected);

    let stats = t.get_sctp_stats().await;
    assert!(
        !stats.contains_key("peer_to_kill"),
        "Disconnected node must not appear in sctp_stats/peers map"
    );
}
