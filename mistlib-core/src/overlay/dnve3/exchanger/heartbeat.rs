use super::DNVE3Exchanger;
use crate::action::OverlayAction;
use crate::config::{Config, DensityEncoding};
use crate::overlay::dnve3::data_store::DNVE3DataStore;
use crate::overlay::dnve3::spatial_density::{SpatialDensityData, SpatialDensityDataByte, Vector3};
use crate::overlay::{OverlayEnvelope, OverlayMessage, OVERLAY_MSG_HEARTBEAT};
use crate::signaling::MessageContent;
use crate::types::{DeliveryMethod, NodeId};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) enum HeartbeatDensityPayload {
    Float(SpatialDensityData),
    Byte(SpatialDensityDataByte),
}

struct LocalDensityInput {
    self_id: NodeId,
    self_pos: Vector3,
    connected_peer_positions: Vec<Vector3>,
}

impl DNVE3Exchanger {
    fn encode_heartbeat_payload(data: &SpatialDensityData, config: &Config) -> Vec<u8> {
        let payload = match config.dnve.density_encoding {
            DensityEncoding::Float => HeartbeatDensityPayload::Float(data.clone()),
            DensityEncoding::Byte => HeartbeatDensityPayload::Byte(data.to_byte_encoded()),
        };
        bincode::serialize(&payload).expect("HeartbeatDensityPayload serialization must not fail")
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

    pub fn update_and_send_heartbeat(
        &self,
        config: &Config,
        connected_nodes: &[NodeId],
    ) -> Vec<OverlayAction> {
        let input = self.local_density_input(connected_nodes);
        let self_density = self.create_self_density(&input, config);
        let self_density_data = self.store_self_density_and_merge(input.self_pos, self_density);
        let payload = Self::encode_heartbeat_payload(&self_density_data, config);

        connected_nodes
            .iter()
            .filter_map(|target| self.heartbeat_action(&input.self_id, target, &payload))
            .collect()
    }

    pub fn handle_heartbeat(&self, from: NodeId, payload: &[u8]) {
        if let Some(data) = Self::decode_heartbeat_payload(payload) {
            self.apply_heartbeat_density(from, data);
        }
    }

    fn local_density_input(&self, connected_nodes: &[NodeId]) -> LocalDensityInput {
        let connected_set = connected_nodes.iter().collect::<HashSet<_>>();
        let store = self.node_store.lock().unwrap();
        let self_id = self.local_node_id.clone();
        let self_pos = store
            .nodes
            .get(&self_id)
            .map(|n| n.position)
            .unwrap_or_else(Vector3::zero);
        let connected_peer_positions = store
            .nodes
            .values()
            .filter(|n| n.id != self_id && connected_set.contains(&n.id))
            .map(|n| n.position)
            .collect();

        LocalDensityInput {
            self_id,
            self_pos,
            connected_peer_positions,
        }
    }

    fn create_self_density(
        &self,
        input: &LocalDensityInput,
        config: &Config,
    ) -> SpatialDensityData {
        self.spatial_utils.create_spatial_density(
            input.self_pos,
            &input.connected_peer_positions,
            config.dnve.distance_layers as usize,
            config.dnve.density_max_range,
        )
    }

    fn store_self_density_and_merge(
        &self,
        self_pos: Vector3,
        self_density: SpatialDensityData,
    ) -> SpatialDensityData {
        let mut store = self.dnve_data_store.lock().unwrap();
        let merged_density = self.merged_density_map(&store, self_pos, &self_density);
        store.self_density = Some(self_density.clone());
        store.merged_density_map = Some(merged_density);
        self_density
    }

    fn merged_density_map(
        &self,
        store: &DNVE3DataStore,
        self_pos: Vector3,
        self_density: &SpatialDensityData,
    ) -> Vec<f32> {
        let mut merged_density = self_density.density_map.clone();

        for info in store.density_peers.values() {
            let projected = self
                .spatial_utils
                .project_spatial_density(&info.data, self_pos);
            for (i, val) in projected.density_map.iter().enumerate() {
                merged_density[i] += val;
            }
        }

        merged_density
    }

    fn heartbeat_action(
        &self,
        from: &NodeId,
        target: &NodeId,
        payload: &[u8],
    ) -> Option<OverlayAction> {
        let envelope = OverlayEnvelope {
            from: from.clone(),
            to: target.clone(),
            hop_count: self.hop_count,
            content: MessageContent::Overlay(OverlayMessage {
                message_type: OVERLAY_MSG_HEARTBEAT,
                payload: payload.to_vec(),
            }),
        };

        let data = bincode::serialize(&envelope)
            .map_err(|e| {
                tracing::warn!(
                    "[DNVE3] failed to serialize heartbeat envelope to {}: {}",
                    target,
                    e
                )
            })
            .ok()?;
        Some(OverlayAction::SendMessage {
            to: target.clone(),
            data: bytes::Bytes::from(data),
            method: DeliveryMethod::Unreliable,
        })
    }

    fn apply_heartbeat_density(&self, from: NodeId, data: SpatialDensityData) {
        self.dnve_data_store
            .lock()
            .unwrap()
            .add_or_update_neighbor(from.clone(), data.clone());
        self.node_store
            .lock()
            .unwrap()
            .update_node_position(from, data.position);
    }
}
