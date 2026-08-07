use super::{
    Config, ConnectionMode, DensityEncoding, IceServer, NodeListExchangeMode, SignalingConfig,
    SpatialPartitionType,
};
use crate::error::MistError;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct FlatConfig {
    signaling_url: Option<String>,
    signaling: Option<SignalingConfig>,
    /// STUN/TURN servers. Without this the flat form could not reach
    /// `webrtc.ice_servers` at all, and the nested `Config` needs every one of
    /// its sections spelled out -- so there was no practical way for a host
    /// application to supply TURN credentials.
    ice_servers: Option<Vec<IceServer>>,
    webrtc_connection_timeout_ms: Option<u32>,
    webrtc_defer_connection_watchdog_until_negotiated: Option<bool>,
    webrtc_buffer_early_ice_candidates: Option<bool>,
    max_connection_count: Option<u32>,
    connection_balancer_interval_seconds: Option<f32>,
    expire_seconds: Option<f32>,
    aoi_range: Option<f32>,
    hop_count: Option<u32>,
    force_disconnect_count: Option<u32>,
    max_message_bytes: Option<u32>,
    storage_max_capacity_mb: Option<u64>,
    storage_spatial_retention_radius: Option<f32>,
    storage_spatial_decay_enabled: Option<bool>,
    storage_spatial_decay_interval_secs: Option<u64>,
    storage_spatial_decay_max_probability: Option<f32>,
    heartbeat_interval_seconds: Option<f32>,
    node_list_interval_seconds: Option<f32>,
    ping_interval_seconds: Option<f32>,
    ping_timeout_count: Option<u32>,
    spatial_distance_layers: Option<u32>,
    spatial_density_resolution: Option<u32>,
    spatial_density_encoding: Option<String>,
    spatial_partition_type: Option<String>,
    direction_threshold: Option<f32>,
    connection_mode: Option<String>,
    node_list_exchange_mode: Option<String>,
}

impl FlatConfig {
    pub(super) fn from_config(c: &Config) -> Self {
        Self {
            signaling_url: Some(c.signaling_url.clone()),
            signaling: Some(c.signaling.clone()),
            ice_servers: Some(c.webrtc.ice_servers.clone()),
            webrtc_connection_timeout_ms: Some(c.webrtc.connection_timeout_ms),
            webrtc_defer_connection_watchdog_until_negotiated: Some(
                c.webrtc.defer_connection_watchdog_until_negotiated,
            ),
            webrtc_buffer_early_ice_candidates: Some(c.webrtc.buffer_early_ice_candidates),
            max_connection_count: Some(c.limits.max_connection_count),
            connection_balancer_interval_seconds: Some(c.intervals.connection_balancer),
            expire_seconds: Some(c.limits.expire_node_seconds),
            aoi_range: Some(c.dnve.aoi_range),
            hop_count: Some(c.limits.hop_count),
            force_disconnect_count: Some(c.limits.force_disconnect_count),
            max_message_bytes: Some(c.limits.max_message_bytes),
            storage_max_capacity_mb: Some(c.storage.max_capacity_mb),
            storage_spatial_retention_radius: Some(c.storage.spatial_retention_radius),
            storage_spatial_decay_enabled: Some(c.storage.spatial_decay_enabled),
            storage_spatial_decay_interval_secs: Some(c.storage.spatial_decay_interval_secs),
            storage_spatial_decay_max_probability: Some(c.storage.spatial_decay_max_probability),
            heartbeat_interval_seconds: Some(c.intervals.heartbeat),
            node_list_interval_seconds: Some(c.intervals.node_list),
            ping_interval_seconds: Some(c.intervals.ping),
            ping_timeout_count: Some(c.limits.ping_timeout_count),
            spatial_distance_layers: Some(c.dnve.distance_layers),
            spatial_density_resolution: Some(c.dnve.density_resolution),
            spatial_density_encoding: Some(c.dnve.density_encoding.as_str().to_owned()),
            spatial_partition_type: Some(c.dnve.spatial_partition_type.as_str().to_owned()),
            direction_threshold: Some(c.dnve.direction_threshold),
            connection_mode: Some(c.dnve.connection_mode.as_str().to_owned()),
            node_list_exchange_mode: Some(c.dnve.node_list_exchange_mode.as_str().to_owned()),
        }
    }

    pub(super) fn apply_to(self, c: &mut Config) -> Result<(), MistError> {
        if let Some(v) = self.signaling_url {
            c.signaling_url = v;
        }
        if let Some(v) = self.signaling {
            c.signaling = v;
        }
        if let Some(v) = self.ice_servers {
            c.webrtc.ice_servers = v;
        }
        if let Some(v) = self.webrtc_connection_timeout_ms {
            if v == 0 {
                return Err(MistError::Config(
                    "webrtcConnectionTimeoutMs must be greater than zero".to_string(),
                ));
            }
            c.webrtc.connection_timeout_ms = v;
        }
        if let Some(v) = self.webrtc_defer_connection_watchdog_until_negotiated {
            c.webrtc.defer_connection_watchdog_until_negotiated = v;
        }
        if let Some(v) = self.webrtc_buffer_early_ice_candidates {
            c.webrtc.buffer_early_ice_candidates = v;
        }
        if let Some(v) = self.max_connection_count {
            c.limits.max_connection_count = v;
        }
        if let Some(v) = self.connection_balancer_interval_seconds {
            c.intervals.connection_balancer = v;
        }
        if let Some(v) = self.expire_seconds {
            c.limits.expire_node_seconds = v;
        }
        if let Some(v) = self.aoi_range {
            c.dnve.aoi_range = v;
        }
        if let Some(v) = self.hop_count {
            c.limits.hop_count = v;
        }
        if let Some(v) = self.force_disconnect_count {
            c.limits.force_disconnect_count = v;
        }
        if let Some(v) = self.max_message_bytes {
            c.limits.max_message_bytes = v;
        }
        if let Some(v) = self.storage_max_capacity_mb {
            c.storage.max_capacity_mb = v;
        }
        if let Some(v) = self.storage_spatial_retention_radius {
            c.storage.spatial_retention_radius = v;
        }
        if let Some(v) = self.storage_spatial_decay_enabled {
            c.storage.spatial_decay_enabled = v;
        }
        if let Some(v) = self.storage_spatial_decay_interval_secs {
            c.storage.spatial_decay_interval_secs = v;
        }
        if let Some(v) = self.storage_spatial_decay_max_probability {
            c.storage.spatial_decay_max_probability = v;
        }
        if let Some(v) = self.heartbeat_interval_seconds {
            c.intervals.heartbeat = v;
        }
        if let Some(v) = self.node_list_interval_seconds {
            c.intervals.node_list = v;
        }
        if let Some(v) = self.ping_interval_seconds {
            c.intervals.ping = v;
        }
        if let Some(v) = self.ping_timeout_count {
            c.limits.ping_timeout_count = v;
        }
        if let Some(v) = self.spatial_distance_layers {
            c.dnve.distance_layers = v;
        }
        if let Some(v) = self.spatial_density_resolution {
            c.dnve.density_resolution = v;
        }
        if let Some(v) = self.spatial_density_encoding {
            c.dnve.density_encoding = DensityEncoding::parse(&v).ok_or_else(|| {
                MistError::Config(format!(
                    "invalid spatialDensityEncoding {v:?}; expected one of: {}",
                    DensityEncoding::variants().join(", ")
                ))
            })?;
        }
        if let Some(v) = self.spatial_partition_type {
            c.dnve.spatial_partition_type = SpatialPartitionType::parse(&v).ok_or_else(|| {
                MistError::Config(format!(
                    "invalid spatialPartitionType {v:?}; expected one of: {}",
                    SpatialPartitionType::variants().join(", ")
                ))
            })?;
        }
        if let Some(v) = self.direction_threshold {
            c.dnve.direction_threshold = v;
        }
        if let Some(v) = self.connection_mode {
            c.dnve.connection_mode = ConnectionMode::parse(&v).ok_or_else(|| {
                MistError::Config(format!(
                    "invalid connectionMode {v:?}; expected one of: {}",
                    ConnectionMode::variants().join(", ")
                ))
            })?;
        }
        if let Some(v) = self.node_list_exchange_mode {
            c.dnve.node_list_exchange_mode = NodeListExchangeMode::parse(&v).ok_or_else(|| {
                MistError::Config(format!(
                    "invalid nodeListExchangeMode {v:?}; expected one of: {}",
                    NodeListExchangeMode::variants().join(", ")
                ))
            })?;
        }
        c.normalize_legacy_signaling();
        Ok(())
    }
}
