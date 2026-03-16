use crate::action::OverlayAction;
use crate::config::Config;
use crate::overlay::dnve3::data_store::DNVE3DataStore;
use crate::overlay::dnve3::spatial_density::{
    SpatialDensityData, SpatialDensityDataByte, SpatialDensityUtils, Vector3,
};
use crate::overlay::node_store::{NodeInfo as NodeStoreNode, NodeStore};
use crate::overlay::routing_table::RoutingTable;
use crate::signaling::{
    MessageContent, OverlayMessage, SignalingEnvelope, OVERLAY_MSG_HEARTBEAT,
    OVERLAY_MSG_NODE_LIST, OVERLAY_MSG_PING, OVERLAY_MSG_PONG, OVERLAY_MSG_REQUEST_NODE_LIST,
};
use crate::stats::STATS;
use crate::types::{DeliveryMethod, NodeId};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use web_time::{Duration, Instant};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) enum HeartbeatDensityPayload {
    Float(SpatialDensityData),
    Byte(SpatialDensityDataByte),
}

#[derive(Clone)]
pub struct DNVE3Exchanger {
    dnve_data_store: Arc<Mutex<DNVE3DataStore>>,
    node_store: Arc<Mutex<NodeStore>>,
    routing_table: Arc<Mutex<RoutingTable>>,
    local_node_id: NodeId,
    spatial_utils: SpatialDensityUtils,
}

impl DNVE3Exchanger {
    fn monotonic_millis() -> u64 {
        static START: OnceLock<Instant> = OnceLock::new();
        let start = START.get_or_init(Instant::now);
        start.elapsed().as_millis() as u64
    }

    fn encode_heartbeat_payload(data: &SpatialDensityData, _config: &Config) -> Vec<u8> {
        let payload = HeartbeatDensityPayload::Byte(data.to_byte_encoded());
        bincode::serialize(&payload).unwrap_or_default()
    }

    fn decode_heartbeat_payload(payload: &[u8]) -> Option<SpatialDensityData> {
        if let Ok(data) = bincode::deserialize::<HeartbeatDensityPayload>(payload) {
            return Some(match data {
                HeartbeatDensityPayload::Float(float_data) => float_data,
                HeartbeatDensityPayload::Byte(byte_data) => {
                    SpatialDensityData::from_byte_encoded(&byte_data)
                }
            });
        }

        bincode::deserialize::<SpatialDensityData>(payload).ok()
    }

    pub fn new(
        dnve_data_store: Arc<Mutex<DNVE3DataStore>>,
        node_store: Arc<Mutex<NodeStore>>,
        routing_table: Arc<Mutex<RoutingTable>>,
        local_node_id: NodeId,
        config: &Config,
    ) -> Self {
        Self {
            dnve_data_store,
            node_store,
            routing_table,
            local_node_id,
            spatial_utils: SpatialDensityUtils::new(config.dnve.density_resolution as usize),
        }
    }

    pub fn delete_old_data(&self, config: &Config) {
        let mut store = self.dnve_data_store.lock().unwrap();
        let mut node_store = self.node_store.lock().unwrap();

        let now = web_time::Instant::now();
        let expire_duration = Duration::from_secs(config.limits.expire_node_seconds as u64);

        let mut to_remove = Vec::new();
        for (id, info) in &store.neighbors {
            if now.duration_since(info.last_message_time) > expire_duration {
                to_remove.push(id.clone());
            }
        }

        for id in to_remove {
            store.remove_neighbor(&id);
        }

        let mut to_remove_nodes = Vec::new();
        for (id, time) in &node_store.last_updated {
            if now.duration_since(*time) > expire_duration {
                to_remove_nodes.push(id.clone());
            }
        }
        for id in to_remove_nodes {
            node_store.nodes.remove(&id);
            node_store.last_updated.remove(&id);
            if let Ok(mut rt) = self.routing_table.lock() {
                rt.remove_node_routes(&id);
                rt.cleanup_routes();
            }
        }
    }

    pub fn update_and_send_heartbeat(
        &self,
        config: &Config,
        connected_nodes: &[NodeId],
    ) -> Vec<OverlayAction> {
        let connected_set: std::collections::HashSet<_> = connected_nodes.iter().collect();

        let (self_id, self_pos, other_nodes) = {
            let store = self.node_store.lock().unwrap();
            let self_id = self.local_node_id.clone();
            let self_pos = store
                .nodes
                .get(&self_id)
                .map(|n| n.position)
                .unwrap_or_else(|| Vector3::zero());
            let other_nodes: Vec<_> = store
                .nodes
                .values()
                .filter(|n| n.id != self_id && connected_set.contains(&n.id))
                .map(|n| n.position)
                .collect();
            (self_id, self_pos, other_nodes)
        };

        let self_density = self.spatial_utils.create_spatial_density(
            self_pos,
            &other_nodes,
            config.dnve.distance_layers as usize,
            config.dnve.density_max_range,
        );

        let self_density_data = {
            let mut store = self.dnve_data_store.lock().unwrap();
            let mut merged_density = self_density.density_map.clone();
            for info in store.neighbors.values() {
                let other_projected = self
                    .spatial_utils
                    .project_spatial_density(&info.data, self_pos);
                for (i, val) in other_projected.density_map.iter().enumerate() {
                    merged_density[i] += val;
                }
            }
            store.self_density = Some(self_density.clone());
            store.merged_density_map = Some(merged_density);
            self_density
        };

        let payload = Self::encode_heartbeat_payload(&self_density_data, config);
        let mut actions = Vec::with_capacity(connected_nodes.len());

        for target in connected_nodes {
            let envelope = SignalingEnvelope {
                from: self_id.clone(),
                to: target.clone(),
                hop_count: 1,
                content: MessageContent::Overlay(OverlayMessage {
                    message_type: OVERLAY_MSG_HEARTBEAT,
                    payload: payload.clone(),
                }),
            };

            let data = bytes::Bytes::from(bincode::serialize(&envelope).unwrap_or_default());
            STATS.add_eval_send(data.len() as u64);
            actions.push(OverlayAction::SendMessage {
                to: target.clone(),
                data,
                method: DeliveryMethod::Unreliable,
            });
        }

        actions
    }

    pub fn handle_heartbeat(&self, from: NodeId, payload: &[u8]) {
        STATS.add_eval_receive(payload.len() as u64);
        if let Some(data) = Self::decode_heartbeat_payload(payload) {
            let mut store = self.dnve_data_store.lock().unwrap();
            store.add_or_update_neighbor(from.clone(), data.clone());

            let mut node_store = self.node_store.lock().unwrap();
            let node = node_store
                .nodes
                .entry(from.clone())
                .or_insert_with(|| NodeStoreNode {
                    id: from.clone(),
                    position: data.position,
                });
            node.position = data.position;
            node_store
                .last_updated
                .insert(from, web_time::Instant::now());
        }
    }

    pub fn send_request_node_list(&self, to: &NodeId) -> Vec<OverlayAction> {
        let envelope = SignalingEnvelope {
            from: self.local_node_id.clone(),
            to: to.clone(),
            hop_count: 1,
            content: MessageContent::Overlay(OverlayMessage {
                message_type: OVERLAY_MSG_REQUEST_NODE_LIST,
                payload: vec![],
            }),
        };

        if let Ok(data) = bincode::serialize(&envelope) {
            STATS.add_eval_send(data.len() as u64);
            vec![OverlayAction::SendMessage {
                to: to.clone(),
                data: bytes::Bytes::from(data),
                method: DeliveryMethod::ReliableOrdered,
            }]
        } else {
            vec![]
        }
    }

    pub fn handle_request_node_list(&self, from: NodeId) -> Vec<OverlayAction> {
        STATS.add_eval_receive(0);
        let all_nodes = {
            let node_store = self.node_store.lock().unwrap();
            let routing_table = self.routing_table.lock().unwrap();
            node_store
                .nodes
                .values()
                .filter(|n| routing_table.connected_nodes.contains(&n.id))
                .cloned()
                .collect::<Vec<_>>()
        };

        if let Ok(payload) = bincode::serialize(&all_nodes) {
            let envelope = SignalingEnvelope {
                from: self.local_node_id.clone(),
                to: from.clone(),
                hop_count: 1,
                content: MessageContent::Overlay(OverlayMessage {
                    message_type: OVERLAY_MSG_NODE_LIST,
                    payload,
                }),
            };

            if let Ok(data) = bincode::serialize(&envelope) {
                STATS.add_eval_send(data.len() as u64);
                return vec![OverlayAction::SendMessage {
                    to: from,
                    data: bytes::Bytes::from(data),
                    method: DeliveryMethod::ReliableOrdered,
                }];
            }
        }
        vec![]
    }

    pub fn handle_node_list(&self, from: NodeId, payload: &[u8]) {
        STATS.add_eval_receive(payload.len() as u64);
        if let Ok(nodes) = bincode::deserialize::<Vec<NodeStoreNode>>(payload) {
            let mut node_store = self.node_store.lock().unwrap();
            let mut routing_table = self.routing_table.lock().unwrap();
            let mut dnve_data_store = self.dnve_data_store.lock().unwrap();

            for node in nodes {
                routing_table.add_routing(node.id.clone(), from.clone(), &self.local_node_id);
                dnve_data_store.update_last_message_time(&node.id);

                let store_node =
                    node_store
                        .nodes
                        .entry(node.id.clone())
                        .or_insert_with(|| NodeStoreNode {
                            id: node.id.clone(),
                            position: node.position,
                        });
                store_node.position = node.position;
                node_store
                    .last_updated
                    .insert(node.id.clone(), web_time::Instant::now());
            }
        }
    }

    pub fn send_ping_all(&self, connected_nodes: &[NodeId]) -> Vec<OverlayAction> {
        let now = Self::monotonic_millis();
        let payload = now.to_le_bytes().to_vec();

        connected_nodes
            .iter()
            .map(|target| {
                let envelope = SignalingEnvelope {
                    from: self.local_node_id.clone(),
                    to: target.clone(),
                    hop_count: 1,
                    content: MessageContent::Overlay(OverlayMessage {
                        message_type: OVERLAY_MSG_PING,
                        payload: payload.clone(),
                    }),
                };

                let data = bincode::serialize(&envelope).unwrap_or_default();
                OverlayAction::SendMessage {
                    to: target.clone(),
                    data: bytes::Bytes::from(data),
                    method: DeliveryMethod::Unreliable,
                }
            })
            .collect()
    }

    pub fn handle_ping(&self, from: NodeId, payload: &[u8]) -> Vec<OverlayAction> {
        let envelope = SignalingEnvelope {
            from: self.local_node_id.clone(),
            to: from.clone(),
            hop_count: 1,
            content: MessageContent::Overlay(OverlayMessage {
                message_type: OVERLAY_MSG_PONG,
                payload: payload.to_vec(),
            }),
        };

        if let Ok(data) = bincode::serialize(&envelope) {
            vec![OverlayAction::SendMessage {
                to: from,
                data: bytes::Bytes::from(data),
                method: DeliveryMethod::Unreliable,
            }]
        } else {
            vec![]
        }
    }

    pub fn handle_pong(&self, from: NodeId, payload: &[u8]) {
        if payload.len() < 8 {
            return;
        }
        let sent_time = u64::from_le_bytes(payload[..8].try_into().unwrap_or_default());
        let now = Self::monotonic_millis();
        let rtt_ms = now.saturating_sub(sent_time) as f32;
        STATS.set_rtt(from, rtt_ms);
    }
}
