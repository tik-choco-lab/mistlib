mod enums;
mod flat;
mod signaling;

#[cfg(test)]
mod tests;

use crate::error::MistError;
pub use enums::{ConnectionMode, DensityEncoding, NodeListExchangeMode, SpatialPartitionType};
use flat::FlatConfig;
use serde::{Deserialize, Serialize};
pub use signaling::{
    NostrSignalingConfig, SignalingConfig, SignalingMode, DEFAULT_NOSTR_RELAY_LIST_URL,
};
use std::f32::consts::PI;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub signaling_url: String,
    #[serde(default)]
    pub signaling: SignalingConfig,
    pub limits: LimitsConfig,
    pub dnve: DnveConfig,
    pub intervals: IntervalsConfig,
    pub webrtc: WebRtcConfig,
    #[serde(default)]
    pub storage: StorageConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LimitsConfig {
    pub max_connection_count: u32,
    pub expire_node_seconds: f32,
    pub hop_count: u32,
    pub reserved_connection_count: u32,
    pub force_disconnect_count: u32,
    /// Consecutive missed PONGs (unanswered before the next PING) before a peer is
    /// logged as a liveness suspect. `0` disables the threshold check entirely.
    #[serde(default = "default_ping_timeout_count")]
    pub ping_timeout_count: u32,
    /// Upper bound (in bytes) on a single Transport::send payload (post-envelope,
    /// pre-wire). Enforced by native/wasm transports; core only carries the value.
    /// See SPEC-13.
    #[serde(default = "default_max_message_bytes")]
    pub max_message_bytes: u32,
}

fn default_ping_timeout_count() -> u32 {
    5
}

fn default_max_message_bytes() -> u32 {
    65536
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DnveConfig {
    pub density_max_range: f32,
    pub distance_layers: u32,
    pub density_resolution: u32,
    #[serde(default)]
    pub density_encoding: DensityEncoding,
    #[serde(default)]
    pub spatial_partition_type: SpatialPartitionType,
    pub direction_threshold: f32,
    pub aoi_range: f32,
    #[serde(default)]
    pub connection_mode: ConnectionMode,
    #[serde(default)]
    pub node_list_exchange_mode: NodeListExchangeMode,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IntervalsConfig {
    pub connection_balancer: f32,
    pub heartbeat: f32,
    pub node_list: f32,
    /// PING keepalive cadence, decoupled from `heartbeat` so it can be tuned
    /// independently (e.g. kept fast for liveness detection while heartbeat
    /// backs off when idle).
    #[serde(default = "default_ping_interval")]
    pub ping: f32,
}

fn default_ping_interval() -> f32 {
    1.0
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WebRtcConfig {
    pub ice_servers: Vec<IceServer>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StorageConfig {
    pub max_capacity_mb: u64,
    /// Protected radius `R` around each self-position: blocks tagged closer
    /// than this are never spatially evicted/decayed. See SPEC-16.
    #[serde(default = "default_spatial_retention_radius")]
    pub spatial_retention_radius: f32,
    /// Enables the periodic decay sweep (driven by native/wasm; core only
    /// exposes `run_decay_sweep`).
    #[serde(default = "default_spatial_decay_enabled")]
    pub spatial_decay_enabled: bool,
    #[serde(default = "default_spatial_decay_interval_secs")]
    pub spatial_decay_interval_secs: u64,
    /// Upper bound on the per-sweep deletion probability for blocks at/beyond
    /// `4 * spatial_retention_radius`.
    #[serde(default = "default_spatial_decay_max_probability")]
    pub spatial_decay_max_probability: f32,
}

fn default_spatial_retention_radius() -> f32 {
    100.0
}

fn default_spatial_decay_enabled() -> bool {
    false
}

fn default_spatial_decay_interval_secs() -> u64 {
    60
}

fn default_spatial_decay_max_probability() -> f32 {
    0.2
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_capacity_mb: 8 * 1024,
            spatial_retention_radius: default_spatial_retention_radius(),
            spatial_decay_enabled: default_spatial_decay_enabled(),
            spatial_decay_interval_secs: default_spatial_decay_interval_secs(),
            spatial_decay_max_probability: default_spatial_decay_max_probability(),
        }
    }
}

/// STUN servers used when the host application configures none.
///
/// Three entries, deliberately: a peer that cannot reach *any* STUN server has
/// no server-reflexive candidate at all and will only connect on the local
/// network. Google's is unreachable from mainland China, and reaching only one
/// operator's is a single point of failure, so the list spans three independent
/// ones. Cloudflare's is anycast and therefore close from most of the world;
/// Xiaomi's is the well-known choice that resolves and answers inside China.
///
/// Kept at three on purpose. Each additional server adds another candidate to
/// gather and another set of pairs for ICE to check, and the marginal value
/// drops off quickly once the geographic gaps are covered.
///
/// None of this helps against symmetric NAT — that needs a TURN relay, which
/// costs real bandwidth and so cannot ship as a default. Host applications that
/// need to traverse it must configure `webrtc.iceServers` with their own TURN
/// credentials.
pub const DEFAULT_STUN_URLS: [&str; 3] = [
    "stun:stun.l.google.com:19302",
    "stun:stun.cloudflare.com:3478",
    "stun:stun.miwifi.com:3478",
];

/// The `webrtc.iceServers` entry used when the host configures none.
pub fn default_ice_servers() -> Vec<IceServer> {
    vec![IceServer {
        urls: DEFAULT_STUN_URLS.iter().map(|u| u.to_string()).collect(),
        username: None,
        credential: None,
    }]
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

impl IceServer {
    /// Whether this entry can safely be handed to a PeerConnection
    /// constructor. Both webrtc-rs (`ErrNoTurnCredentials`) and browsers
    /// (`InvalidAccessError`, per the WebRTC spec) reject a turn/turns URL
    /// without a non-empty username and credential *at construction time* —
    /// so a single bad entry would permanently fail every subsequent
    /// connection attempt in the session. Callers (native `map_ice_servers`,
    /// wasm `build_ice_server_plans`) must drop unusable entries, with a
    /// warning, instead of forwarding them.
    ///
    /// An entry with no URLs is also unusable: browsers reject an empty
    /// `urls` array outright.
    pub fn is_usable(&self) -> bool {
        if self.urls.is_empty() {
            return false;
        }
        let needs_credentials = self.urls.iter().any(|url| {
            let scheme = url.trim_start().split(':').next().unwrap_or("");
            scheme.eq_ignore_ascii_case("turn") || scheme.eq_ignore_ascii_case("turns")
        });
        !needs_credentials
            || (self.username.as_deref().is_some_and(|u| !u.is_empty())
                && self.credential.as_deref().is_some_and(|c| !c.is_empty()))
    }
}

impl Config {
    const AUTO_DIRECTION_THRESHOLD_SENTINEL: f32 = 0.0;

    pub fn new_default() -> Self {
        Self {
            signaling_url: "wss://rtc.tik-choco.com/signaling".to_string(),
            signaling: SignalingConfig::default(),
            limits: LimitsConfig {
                max_connection_count: 30,
                expire_node_seconds: 10.0,
                hop_count: 2,
                reserved_connection_count: 1,
                force_disconnect_count: 0,
                ping_timeout_count: default_ping_timeout_count(),
                max_message_bytes: default_max_message_bytes(),
            },
            dnve: DnveConfig {
                density_max_range: 64.0,
                distance_layers: 1,
                density_resolution: 6,
                density_encoding: DensityEncoding::Byte,
                spatial_partition_type: SpatialPartitionType::Dodecahedron,
                direction_threshold: Self::AUTO_DIRECTION_THRESHOLD_SENTINEL,
                aoi_range: 10.0,
                connection_mode: ConnectionMode::NodeListAoiGuard,
                node_list_exchange_mode: NodeListExchangeMode::Pull,
            },
            intervals: IntervalsConfig {
                connection_balancer: 2.0,
                heartbeat: 1.0,
                node_list: 2.0,
                ping: default_ping_interval(),
            },
            webrtc: WebRtcConfig {
                ice_servers: default_ice_servers(),
            },
            storage: StorageConfig::default(),
        }
    }

    pub fn update_from_json(&mut self, json_str: &str) -> crate::error::Result<()> {
        let full_config_err = match serde_json::from_str::<Config>(json_str) {
            Ok(mut new_config) => {
                new_config.normalize_legacy_signaling();
                if !new_config.validate_signaling() {
                    return Err(MistError::Config(
                        "invalid signaling configuration".to_string(),
                    ));
                }
                *self = new_config;
                return Ok(());
            }
            Err(e) => e,
        };

        match serde_json::from_str::<FlatConfig>(json_str) {
            Ok(flat) => {
                let mut next = self.clone();
                flat.apply_to(&mut next)?;
                if !next.validate_signaling() {
                    return Err(MistError::Config(
                        "invalid signaling configuration".to_string(),
                    ));
                }
                *self = next;
                Ok(())
            }
            Err(flat_config_err) => Err(MistError::Config(format!(
                "failed to parse as full Config ({full_config_err}) or as flat config ({flat_config_err})"
            ))),
        }
    }

    pub fn use_websocket_signaling_url(&mut self, signaling_url: String) {
        self.signaling_url = signaling_url;
        self.signaling = SignalingConfig::websocket();
    }

    pub fn validate_signaling(&self) -> bool {
        match self.signaling.mode {
            SignalingMode::WebSocket => true,
            SignalingMode::Nostr => self
                .signaling
                .nostr
                .as_ref()
                .is_some_and(NostrSignalingConfig::validate),
        }
    }

    fn normalize_legacy_signaling(&mut self) {
        if self.signaling.mode == SignalingMode::WebSocket {
            self.signaling.nostr = None;
        }
    }

    pub fn effective_direction_threshold(&self) -> f32 {
        Self::effective_direction_threshold_for(
            self.dnve
                .spatial_partition_type
                .direction_count(self.dnve.density_resolution),
            self.dnve.direction_threshold,
        )
    }

    pub fn effective_direction_threshold_for(
        density_resolution: u32,
        configured_threshold: f32,
    ) -> f32 {
        if (0.0..=1.0).contains(&configured_threshold)
            && configured_threshold > Self::AUTO_DIRECTION_THRESHOLD_SENTINEL
        {
            return configured_threshold;
        }

        let direction_count = density_resolution.max(1) as f32;
        let spherical_cap_area = 4.0 * PI / direction_count;
        let cap_cos = (1.0 - spherical_cap_area / (2.0 * PI)).clamp(-1.0, 1.0);
        let cap_half_angle = cap_cos.acos();

        // Shrink the ideal equal-area cone slightly so neighboring directions overlap less.
        let tuned_half_angle = (cap_half_angle * 0.9).clamp(0.0, PI);
        tuned_half_angle.cos().clamp(0.0, 0.999_999)
    }

    pub fn to_json_string(&self) -> String {
        let flat = FlatConfig::from_config(self);
        serde_json::to_string(&flat).unwrap_or_else(|_| "{}".to_string())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new_default()
    }
}
