use super::common::{
    extract_node_list_payload, insert_pos, make_exchanger_full, node, pos, test_config,
};
use crate::config::ConnectionMode;

// トポロジー: node1 — node2 — node3 — node4 — node5
//
// node1 は node2 とのみ直接接続し、node3/node4/node5 とは非接続。
// NODE_LIST 交換で得た情報を段階的に取り込んだ後、node1 の aoi_nodes に
// 非接続ノードが表示されることを確認する。
#[test]
fn node_list_exchange_non_connected_nodes_appear_in_aoi() {
    let mut config = test_config();
    config.dnve.connection_mode = ConnectionMode::NodeListProximity;
    config.dnve.aoi_range = 1000.0;

    let (ex1, ds1, ns1, rt1) = make_exchanger_full("node1", &config);
    let (ex2, _ds2, ns2, rt2) = make_exchanger_full("node2", &config);
    let (ex3, _ds3, ns3, rt3) = make_exchanger_full("node3", &config);
    let (ex4, _ds4, ns4, rt4) = make_exchanger_full("node4", &config);

    // node1: node2 と直接接続
    insert_pos(&ns1, "node1", pos(0.0, 0.0, 0.0));
    insert_pos(&ns1, "node2", pos(10.0, 0.0, 0.0));
    rt1.lock().unwrap().on_connected(node("node2"));

    // node2: node1, node3 と直接接続
    insert_pos(&ns2, "node1", pos(0.0, 0.0, 0.0));
    insert_pos(&ns2, "node2", pos(10.0, 0.0, 0.0));
    insert_pos(&ns2, "node3", pos(20.0, 0.0, 0.0));
    rt2.lock().unwrap().on_connected(node("node1"));
    rt2.lock().unwrap().on_connected(node("node3"));

    // node3: node2, node4 と直接接続
    insert_pos(&ns3, "node2", pos(10.0, 0.0, 0.0));
    insert_pos(&ns3, "node3", pos(20.0, 0.0, 0.0));
    insert_pos(&ns3, "node4", pos(30.0, 0.0, 0.0));
    rt3.lock().unwrap().on_connected(node("node2"));
    rt3.lock().unwrap().on_connected(node("node4"));

    // node4: node3, node5 と直接接続
    insert_pos(&ns4, "node3", pos(20.0, 0.0, 0.0));
    insert_pos(&ns4, "node4", pos(30.0, 0.0, 0.0));
    insert_pos(&ns4, "node5", pos(40.0, 0.0, 0.0));
    rt4.lock().unwrap().on_connected(node("node3"));
    rt4.lock().unwrap().on_connected(node("node5"));

    // ── Step 1: node1 が node2 から NODE_LIST を受け取る (node3 を知る) ──
    let actions = ex2.handle_request_node_list(node("node1"), ConnectionMode::NodeListProximity);
    assert_eq!(actions.len(), 1, "node2 は node_list 応答を 1 つ返すべき");
    ex1.handle_node_list(&node("node2"), &extract_node_list_payload(&actions[0]));

    assert!(
        ds1.lock().unwrap().aoi_nodes.contains(&node("node3")),
        "1-hop node-list exchange 後: node3 が node1 の aoi_nodes に表示されるべき"
    );
    assert_eq!(
        rt1.lock().unwrap().get_next_hop(&node("node3")),
        Some(node("node2")),
        "node3 への next_hop は直接接続中の node2 であるべき (1-hop relay)"
    );

    // ── Step 2: node1 が node3 から NODE_LIST を受け取る (node4 を知る) ──
    let actions = ex3.handle_request_node_list(node("node1"), ConnectionMode::NodeListProximity);
    assert_eq!(actions.len(), 1, "node3 は node_list 応答を 1 つ返すべき");
    ex1.handle_node_list(&node("node3"), &extract_node_list_payload(&actions[0]));

    assert!(
        ds1.lock().unwrap().aoi_nodes.contains(&node("node4")),
        "2-hop node-list exchange 後: node4 が node1 の aoi_nodes に表示されるべき"
    );
    assert_eq!(
        rt1.lock().unwrap().get_next_hop(&node("node4")),
        None,
        "node4 は 2-hop 先のため node1 からシグナリング中継はできないが、表示はされるべき"
    );

    // ── Step 3: node1 が node4 から NODE_LIST を受け取る (node5 を知る) ──
    let actions = ex4.handle_request_node_list(node("node1"), ConnectionMode::NodeListProximity);
    assert_eq!(actions.len(), 1, "node4 は node_list 応答を 1 つ返すべき");
    ex1.handle_node_list(&node("node4"), &extract_node_list_payload(&actions[0]));

    assert!(
        ds1.lock().unwrap().aoi_nodes.contains(&node("node5")),
        "3-hop node-list exchange 後: node5 が node1 の aoi_nodes に表示されるべき"
    );
}
