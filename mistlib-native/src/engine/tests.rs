use super::*;
use mistlib_core::stats::STATS;
use mistlib_core::types::NodeId;

#[tokio::test]
async fn test_engine_get_stats_json_structure() {
    STATS.add_send(100);
    STATS.add_receive(50);
    STATS.add_eval_send(200);
    STATS.set_rtt(NodeId("peer-1".to_string()), 15.5);

    let stats_json = ENGINE.get_stats_json().await;
    let stats: serde_json::Value = serde_json::from_str(&stats_json).unwrap();

    assert_eq!(stats["sendBits"], 100 * 8);
    assert_eq!(stats["receiveBits"], 50 * 8);
    assert_eq!(stats["evalSendBits"], 200 * 8);
    assert_eq!(stats["rttMillis"]["peer-1"], 15.5);
    assert!(stats["nodes"].is_array());

    let next_stats_json = ENGINE.get_stats_json().await;
    let next_stats: serde_json::Value = serde_json::from_str(&next_stats_json).unwrap();
    assert_eq!(next_stats["sendBits"], 0);
}
