use super::*;

fn topology_rank(signaler: &NostrSignaler, room_id: &str) -> String {
    signaler
        .codec_config
        .topology_rank(room_id, &signaler.identity.public_key)
}

fn sort_by_topology_rank(room_id: &str, nodes: &mut Vec<&NostrSignaler>) {
    nodes.sort_by_key(|node| {
        (
            topology_rank(node, room_id),
            node.identity.public_key.clone(),
        )
    });
}

fn event_from_published_frame(frame: &str) -> NostrEvent {
    let value: serde_json::Value = serde_json::from_str(frame).unwrap();
    let items = value.as_array().expect("relay frame must be an array");
    assert_eq!(
        items.first().and_then(serde_json::Value::as_str),
        Some("EVENT")
    );
    serde_json::from_value(items[1].clone()).unwrap()
}

async fn assert_direct_request_from(
    rx: &mut mpsc::Receiver<String>,
    sender: &NostrSignaler,
    receiver: &NostrSignaler,
    receiver_id: &NodeId,
    room_id: &str,
) {
    let frame = rx.try_recv().expect("expected direct Request frame");
    let event = event_from_published_frame(&frame);
    let decoded = decode_message_event(
        &sender.codec_config,
        &sender.crypto,
        &receiver.identity,
        receiver_id,
        &event,
        room_id,
    )
    .unwrap();

    assert_eq!(decoded.sender_pubkey, sender.identity.public_key);
    assert_eq!(decoded.data.sender_id, sender.local_node_id);
    assert_eq!(decoded.data.receiver_id, NodeId::broadcast());
    assert_eq!(decoded.data.signaling_type, SignalingType::Request);
}

async fn seed_known_peers(
    local: &NostrSignaler,
    ranked: &[&NostrSignaler],
    subject: &NostrSignaler,
    room_id: &str,
) {
    let mut table = local.discovery_table.lock().await;
    for peer in ranked {
        if peer.identity.public_key == local.identity.public_key
            || peer.identity.public_key == subject.identity.public_key
        {
            continue;
        }
        table.insert_pubkey_with_rank(
            peer.identity.public_key.clone(),
            u64::MAX,
            topology_rank(peer, room_id),
        );
    }
}

#[tokio::test]
async fn only_two_ranked_responders_request_discovered_peer() {
    let room_id = "room-a";
    let nodes: Vec<NostrSignaler> = (b'A'..=b'E')
        .map(|label| NostrSignaler::new(NodeId((label as char).to_string()), config()))
        .collect();
    let mut ranked = nodes.iter().collect::<Vec<_>>();
    sort_by_topology_rank(room_id, &mut ranked);
    let subject = ranked[3];
    let responders = [ranked[2], ranked[1]];
    let non_responder = ranked[0];
    let discovery = build_discovery_event(
        &subject.codec_config,
        &subject.crypto,
        &subject.identity,
        room_id,
    )
    .unwrap();

    for responder in responders {
        responder.set_room_id(room_id).await.unwrap();
        seed_known_peers(responder, &ranked, subject, room_id).await;
        let (tx, mut rx) = mpsc::channel(4);
        responder.senders.lock().await.push(tx);
        responder
            .process_event(discovery.clone(), mpsc::channel(1).0)
            .await
            .unwrap();

        assert_direct_request_from(&mut rx, responder, subject, &subject.local_node_id, room_id)
            .await;
        assert!(rx.try_recv().is_err(), "responder must send one request");
    }

    non_responder.set_room_id(room_id).await.unwrap();
    seed_known_peers(non_responder, &ranked, subject, room_id).await;
    let (tx, mut rx) = mpsc::channel(4);
    non_responder.senders.lock().await.push(tx);
    non_responder
        .process_event(discovery, mpsc::channel(1).0)
        .await
        .unwrap();
    assert!(
        rx.try_recv().is_err(),
        "non responder must not request discovered peer"
    );
}

#[tokio::test]
async fn higher_rank_node_requests_discovered_predecessor() {
    let room_id = "room-a";
    let a = NostrSignaler::new(NodeId("A".to_string()), config());
    let b = NostrSignaler::new(NodeId("B".to_string()), config());
    let mut ranked = vec![&a, &b];
    sort_by_topology_rank(room_id, &mut ranked);
    let subject = ranked[0];
    let local = ranked[1];

    local.set_room_id(room_id).await.unwrap();

    let (tx, mut rx) = mpsc::channel(4);
    local.senders.lock().await.push(tx);

    let discovery = build_discovery_event(
        &subject.codec_config,
        &subject.crypto,
        &subject.identity,
        room_id,
    )
    .unwrap();

    local
        .process_event(discovery, mpsc::channel(1).0)
        .await
        .unwrap();

    assert_direct_request_from(&mut rx, local, subject, &subject.local_node_id, room_id).await;
    assert!(
        rx.try_recv().is_err(),
        "local must send exactly one request"
    );
}

fn ordered_edge(left: &NodeId, right: &NodeId) -> (String, String) {
    if left.0 <= right.0 {
        (left.0.clone(), right.0.clone())
    } else {
        (right.0.clone(), left.0.clone())
    }
}

async fn drain_request_edges(
    receivers: &mut [(NodeId, mpsc::Receiver<MessageContent>)],
    edges: &mut std::collections::BTreeSet<(String, String)>,
) {
    for (receiver_id, rx) in receivers.iter_mut() {
        loop {
            match timeout(Duration::from_millis(25), rx.recv()).await {
                Ok(Some(MessageContent::Data(data)))
                    if data.signaling_type == SignalingType::Request =>
                {
                    edges.insert(ordered_edge(&data.sender_id, receiver_id));
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }
}

fn graph_is_connected(
    ids: &[NodeId],
    edges: &std::collections::BTreeSet<(String, String)>,
) -> bool {
    let Some(first) = ids.first() else {
        return true;
    };
    let mut seen = std::collections::BTreeSet::from([first.0.clone()]);
    let mut changed = true;
    while changed {
        changed = false;
        for (left, right) in edges {
            if seen.contains(left) && seen.insert(right.clone()) {
                changed = true;
            }
            if seen.contains(right) && seen.insert(left.clone()) {
                changed = true;
            }
        }
    }
    ids.iter().all(|id| seen.contains(&id.0))
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn has_cross_edge(
    edges: &std::collections::BTreeSet<(String, String)>,
    local_prefix: &str,
    remote_prefix: &str,
) -> bool {
    edges.iter().any(|(left, right)| {
        (left.starts_with(local_prefix) && right.starts_with(remote_prefix))
            || (left.starts_with(remote_prefix) && right.starts_with(local_prefix))
    })
}

#[tokio::test]
#[ignore = "requires `just nostr-relay` or MIST_NOSTR_RELAY_URL"]
async fn live_go_relay_six_nodes_secret_rank_topology() {
    let room_id = format!("nostr-six-room-{}", random_subscription_id());
    let ids: Vec<NodeId> = (b'A'..=b'F')
        .map(|label| NodeId(format!("node-{}-{}", label as char, room_id)))
        .collect();
    let nodes: Vec<NostrSignaler> = ids
        .iter()
        .cloned()
        .map(|id| NostrSignaler::new(id, config()))
        .collect();
    let mut receivers = Vec::new();

    for (id, node) in ids.iter().zip(nodes.iter()) {
        let (tx, rx) = mpsc::channel(64);
        node.connect(tx).await.expect("node connects to relay");
        receivers.push((id.clone(), rx));
    }

    let mut edges = std::collections::BTreeSet::new();
    for node in &nodes {
        node.set_room_id(&room_id).await.unwrap();
        node.publish_discovery(&room_id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        drain_request_edges(&mut receivers, &mut edges).await;
    }

    assert!(graph_is_connected(&ids, &edges));
    assert!(
        edges.len() <= nodes.len() * 2,
        "secret-rank bootstrap should stay sparse, got {} edges: {:?}",
        edges.len(),
        edges
    );
}

async fn discover_until_edge(
    nodes: &[&NostrSignaler],
    receivers: &mut [(NodeId, mpsc::Receiver<MessageContent>)],
    room_id: &str,
    edge: &(String, String),
    rounds: usize,
) -> bool {
    let mut edges = std::collections::BTreeSet::new();
    for _ in 0..rounds {
        for node in nodes {
            node.set_room_id(room_id).await.unwrap();
            node.publish_discovery(room_id).await.unwrap();
            tokio::time::sleep(Duration::from_millis(175)).await;
            drain_request_edges(receivers, &mut edges).await;
        }
        if edges.contains(edge) {
            return true;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    drain_request_edges(receivers, &mut edges).await;
    edges.contains(edge)
}

async fn discover_rounds(
    nodes: &[&NostrSignaler],
    receivers: &mut [(NodeId, mpsc::Receiver<MessageContent>)],
    room_id: &str,
    edges: &mut std::collections::BTreeSet<(String, String)>,
    rounds: usize,
) {
    for _ in 0..rounds {
        for node in nodes {
            node.set_room_id(room_id).await.unwrap();
            node.publish_discovery(room_id).await.unwrap();
            tokio::time::sleep(Duration::from_millis(175)).await;
            drain_request_edges(receivers, edges).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        drain_request_edges(receivers, edges).await;
    }
}

#[tokio::test]
#[ignore = "requires `just nostr-relay` or MIST_NOSTR_RELAY_URL"]
async fn live_go_relay_six_nodes_reconnect_one_peer_keeps_graph_connected() {
    let room_id = format!("nostr-six-reconnect-room-{}", random_subscription_id());
    let ids: Vec<NodeId> = (b'A'..=b'F')
        .map(|label| NodeId(format!("six-reconnect-node-{}-{}", label as char, room_id)))
        .collect();
    let nodes: Vec<NostrSignaler> = ids
        .iter()
        .cloned()
        .map(|id| NostrSignaler::new(id, config()))
        .collect();
    let mut receivers = Vec::new();
    for (id, node) in ids.iter().zip(nodes.iter()) {
        let (tx, rx) = mpsc::channel(64);
        node.connect(tx).await.expect("node connects to relay");
        receivers.push((id.clone(), rx));
    }

    // Bootstrap the full six-node graph.
    let node_refs: Vec<&NostrSignaler> = nodes.iter().collect();
    let mut edges = std::collections::BTreeSet::new();
    discover_rounds(&node_refs, &mut receivers, &room_id, &mut edges, 3).await;
    assert!(
        graph_is_connected(&ids, &edges),
        "six-node graph should be connected before reconnect, got edges: {:?}",
        edges
    );

    // One peer (node D) disconnects and reconnects on the SAME instance.
    // Its prior links are gone; the five survivors keep their existing edges,
    // so model that by dropping every bootstrap edge that touched the victim.
    let victim = 3usize;
    let victim_id = ids[victim].0.clone();
    edges.retain(|(left, right)| *left != victim_id && *right != victim_id);

    nodes[victim].close().await.expect("victim disconnects");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (victim_tx, victim_rx) = mpsc::channel(64);
    nodes[victim]
        .connect(victim_tx)
        .await
        .expect("victim reconnects");
    receivers[victim] = (ids[victim].clone(), victim_rx);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Re-discover: the rejoined peer must re-acquire edges and the union of
    // surviving edges plus the new ones must keep the whole graph connected.
    discover_rounds(&node_refs, &mut receivers, &room_id, &mut edges, 4).await;
    assert!(
        edges
            .iter()
            .any(|(left, right)| *left == victim_id || *right == victim_id),
        "rejoined peer {} should re-acquire at least one signaling edge, got edges: {:?}",
        victim_id,
        edges
    );
    assert!(
        graph_is_connected(&ids, &edges),
        "six-node graph should stay connected after one peer rejoins, got edges: {:?}",
        edges
    );

    for node in nodes {
        node.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires `just nostr-relay` or MIST_NOSTR_RELAY_URL"]
async fn live_go_relay_peer_reconnects_after_disconnect() {
    let room_id = format!("nostr-reconnect-room-{}", random_subscription_id());
    let alice_id = NodeId(format!("reconnect-alice-{room_id}"));
    let bob_id = NodeId(format!("reconnect-bob-{room_id}"));
    let expected_edge = ordered_edge(&alice_id, &bob_id);

    let alice = NostrSignaler::new(alice_id.clone(), config());
    let bob = NostrSignaler::new(bob_id.clone(), config());

    // First connection: both peers join and should discover each other.
    let (alice_tx, alice_rx) = mpsc::channel(64);
    let (bob_tx, bob_rx) = mpsc::channel(64);
    alice.connect(alice_tx).await.expect("alice connects");
    bob.connect(bob_tx).await.expect("bob connects");
    let mut receivers = vec![(alice_id.clone(), alice_rx), (bob_id.clone(), bob_rx)];

    assert!(
        discover_until_edge(&[&alice, &bob], &mut receivers, &room_id, &expected_edge, 3).await,
        "alice and bob should form a signaling edge on first connect",
    );

    // Bob disconnects.
    bob.close().await.expect("bob disconnects");
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Drop bob's stale receiver so reconnection uses a fresh channel.
    receivers.pop();

    // Bob reconnects with a fresh inbound channel.
    let (bob_tx2, bob_rx2) = mpsc::channel(64);
    bob.connect(bob_tx2).await.expect("bob reconnects");
    receivers.push((bob_id.clone(), bob_rx2));
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        discover_until_edge(&[&alice, &bob], &mut receivers, &room_id, &expected_edge, 4).await,
        "alice and bob should re-form a signaling edge after bob reconnects",
    );

    alice.close().await.unwrap();
    bob.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires `just nostr-relay` or MIST_NOSTR_RELAY_URL"]
async fn live_go_relay_mixed_six_native_nodes_connect_to_wasm_nodes() {
    let room_id = env_string("MIST_NOSTR_ROOM_ID")
        .unwrap_or_else(|| format!("nostr-mixed-six-room-{}", random_subscription_id()));
    let ids: Vec<NodeId> = (b'A'..=b'F')
        .map(|label| NodeId(format!("native-node-{}-{}", label as char, room_id)))
        .collect();
    let nodes: Vec<NostrSignaler> = ids
        .iter()
        .cloned()
        .map(|id| NostrSignaler::new(id, config()))
        .collect();
    let mut receivers = Vec::new();

    for (id, node) in ids.iter().zip(nodes.iter()) {
        let (tx, rx) = mpsc::channel(64);
        node.connect(tx)
            .await
            .expect("native node connects to relay");
        receivers.push((id.clone(), rx));
    }

    tokio::time::sleep(Duration::from_millis(3_500)).await;

    let mut edges = std::collections::BTreeSet::new();
    for _round in 0..2 {
        for node in &nodes {
            node.set_room_id(&room_id).await.unwrap();
            node.publish_discovery(&room_id).await.unwrap();
            tokio::time::sleep(Duration::from_millis(175)).await;
            drain_request_edges(&mut receivers, &mut edges).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        drain_request_edges(&mut receivers, &mut edges).await;
    }

    tokio::time::sleep(Duration::from_millis(1_000)).await;
    drain_request_edges(&mut receivers, &mut edges).await;

    assert!(
        graph_is_connected(&ids, &edges),
        "native mixed six-node Nostr signaling graph should be connected, got edges: {:?}",
        edges
    );
    assert!(
        has_cross_edge(&edges, "native-node-", "wasm-node-"),
        "native nodes should receive at least one request edge from WASM nodes, got edges: {:?}",
        edges
    );

    for node in nodes {
        node.close().await.unwrap();
    }
}
