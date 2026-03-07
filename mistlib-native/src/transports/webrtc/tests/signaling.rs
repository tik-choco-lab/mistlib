use super::*;
use mistlib_core::signaling::{MessageContent, SignalingData, SignalingHandler, SignalingType};
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, NodeId};

#[tokio::test]
async fn handle_message_request_does_not_crash() {
    let t = make_transport();
    let msg = MessageContent::Data(SignalingData {
        sender_id: NodeId("remote".to_string()),
        receiver_id: NodeId("local".to_string()),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: String::new(),
    });
    assert!(t.handle_message(msg).await.is_ok());
}

#[tokio::test]
async fn handle_message_request_at_max_does_not_crash() {
    use mistlib_core::signaling::SignalingHandler;
    let t = make_transport();
    t.set_max_connections(0);
    let msg = MessageContent::Data(SignalingData {
        sender_id: NodeId("zzz".to_string()),
        receiver_id: NodeId("local".to_string()),
        room_id: String::new(),
        signaling_type: SignalingType::Request,
        data: String::new(),
    });
    let result = t.handle_message(msg).await;
    assert!(result.is_ok());
    assert_eq!(
        t.get_active_connection_states().len(),
        0,
        "No connection must be added when max=0"
    );
}

#[tokio::test]
async fn handle_message_unknown_type_does_not_crash() {
    use mistlib_core::signaling::SignalingHandler;
    let t = make_transport();
    let msg = MessageContent::Raw(b"garbage".to_vec());
    assert!(t.handle_message(msg).await.is_ok());
}

#[tokio::test]
async fn many_concurrent_requests_do_not_crash() {
    use mistlib_core::signaling::SignalingHandler;
    use std::sync::Arc as StdArc;
    const MAX: u32 = 3;
    let t = StdArc::new(make_transport());
    t.set_max_connections(MAX);

    let mut handles = Vec::new();
    for i in 0..20u32 {
        let tc = StdArc::clone(&t);
        handles.push(tokio::spawn(async move {
            let msg = MessageContent::Data(SignalingData {
                sender_id: NodeId(format!("zzz-{i}")),
                receiver_id: NodeId("local".to_string()),
                room_id: String::new(),
                signaling_type: SignalingType::Request,
                data: String::new(),
            });
            let _ = tc.handle_message(msg).await;
        }));
    }
    for h in handles {
        assert!(h.await.is_ok(), "task must not panic");
    }

    let count = t.get_active_connection_states().len();
    assert!(
        count <= MAX as usize,
        "active ({count}) must not exceed max ({MAX})"
    );
}

#[tokio::test]
async fn connection_state_is_not_connected_after_failed_attempt() {
    let t = make_transport();
    let node = NodeId("unreachable".to_string());
    let _ = t.connect(&node).await;
    assert_ne!(
        t.get_connection_state(&node),
        ConnectionState::Connected,
        "Node should not be Connected after failed WebRTC setup"
    );
}

#[tokio::test]
async fn incoming_request_sequential_respects_max_limit() {
    use mistlib_core::signaling::SignalingHandler;
    let t = make_transport();
    t.set_max_connections(2);

    for i in 1..=3 {
        let msg = MessageContent::Data(SignalingData {
            sender_id: NodeId(format!("remote_{i}")),
            receiver_id: NodeId("local".to_string()),
            room_id: String::new(),
            signaling_type: SignalingType::Request,
            data: String::new(),
        });
        let _ = t.handle_message(msg).await;
    }

    let active = t.get_active_connection_states();
    assert_eq!(
        active.len(),
        2,
        "Even with 3 incoming requests, max capacity (2) must be strictly respected"
    );
}

#[tokio::test]
async fn incoming_request_stress_limit_with_wait() {
    use mistlib_core::signaling::SignalingHandler;
    use tokio::time::sleep;
    use web_time::Duration;

    let t = make_transport();
    const MAX: u32 = 30;
    t.set_max_connections(MAX);

    for i in 0..50 {
        let msg = MessageContent::Data(SignalingData {
            sender_id: NodeId(format!("stress-remote-{}", i)),
            receiver_id: NodeId("local".to_string()),
            room_id: String::new(),
            signaling_type: SignalingType::Request,
            data: String::new(),
        });
        let _ = t.handle_message(msg).await;
    }

    sleep(Duration::from_millis(100)).await;

    let active = t.get_active_connection_states();
    assert_eq!(
        active.len(),
        MAX as usize,
        "Active connections must be strictly limited to {} even after waiting and 10 incoming requests",
        MAX
    );
}
