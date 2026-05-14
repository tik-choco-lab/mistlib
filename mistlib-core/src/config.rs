use serde::{Deserialize, Serialize};
use std::f32::consts::PI;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DensityEncoding {
    Float,
    #[default]
    Byte,
}

/// 球面方向の分割方式。
///
/// - `Fibonacci`   : `spatialDensityResolution` 個の方向をフィボナッチ球で生成する
/// - 正多面体各種 : 正多面体の面法線方向を代表方向として使う
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpatialPartitionType {
    Fibonacci,
    Tetrahedron,
    Cube,
    Octahedron,
    #[default]
    Dodecahedron,
    Icosahedron,
}

/// 接続モード
///
/// - `DirectionDensity`     : 方向密度マップを交換し、AOI guard + 方向 + density score で接続する
/// - `DirectionDensityLight`: 方向密度マップを交換し、NodeListDirectional + density score で接続する
/// - `NodeListDirectional`  : self を含む NodeList 交換。接続選択は方向ごとの最近傍
/// - `NodeListAoiGuard`     : self と AOI 補助付き NodeList 交換。接続選択は AOI guard + 方向ごとの最近傍
/// - `NodeListAoiProximity` : self と AOI 補助付き NodeList 交換。接続選択は AOI guard + 近距離順
/// - `NodeListAoiDensity`   : NodeListAoiGuard + density score。最小分断対策モード
/// - `NodeListProximity`    : self を含む NodeList 交換。接続選択は近距離順（方向制約なし）
/// - `PSense`               : self と AOI 補助付き NodeList 交換。AOI 内 near node + AOI 外 sector sensor node を選ぶ
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    DirectionDensity,
    DirectionDensityLight,
    NodeListDirectional,
    #[default]
    NodeListAoiGuard,
    NodeListAoiProximity,
    NodeListAoiDensity,
    NodeListProximity,
    PSense,
}

/// NodeList の交換方式。
///
/// - `Pull`: 接続中 peer に REQUEST_NODE_LIST を送り、NODE_LIST 応答を受け取る
/// - `Push`: 接続中 peer に NODE_LIST を直接送る
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NodeListExchangeMode {
    #[default]
    Pull,
    Push,
}

impl ConnectionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectionDensity => "direction_density",
            Self::DirectionDensityLight => "direction_density_light",
            Self::NodeListDirectional => "node_list_directional",
            Self::NodeListAoiGuard => "node_list_aoi_guard",
            Self::NodeListAoiProximity => "node_list_aoi_proximity",
            Self::NodeListAoiDensity => "node_list_aoi_density",
            Self::NodeListProximity => "node_list_proximity",
            Self::PSense => "p_sense",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "direction_density" => Some(Self::DirectionDensity),
            "direction_density_light" => Some(Self::DirectionDensityLight),
            "node_list_directional" => Some(Self::NodeListDirectional),
            "node_list_aoi_guard" => Some(Self::NodeListAoiGuard),
            "node_list_aoi_proximity" => Some(Self::NodeListAoiProximity),
            "node_list_aoi_density" => Some(Self::NodeListAoiDensity),
            "node_list_proximity" => Some(Self::NodeListProximity),
            "p_sense" | "psense" => Some(Self::PSense),
            _ => None,
        }
    }
}

impl NodeListExchangeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pull => "pull",
            Self::Push => "push",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "pull" => Some(Self::Pull),
            "push" => Some(Self::Push),
            _ => None,
        }
    }
}

impl DensityEncoding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Float => "float",
            Self::Byte => "byte",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "float" => Some(Self::Float),
            "byte" => Some(Self::Byte),
            _ => None,
        }
    }
}

impl SpatialPartitionType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fibonacci => "fibonacci",
            Self::Tetrahedron => "tetrahedron",
            Self::Cube => "cube",
            Self::Octahedron => "octahedron",
            Self::Dodecahedron => "dodecahedron",
            Self::Icosahedron => "icosahedron",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "fibonacci" => Some(Self::Fibonacci),
            "tetrahedron" => Some(Self::Tetrahedron),
            "cube" => Some(Self::Cube),
            "octahedron" => Some(Self::Octahedron),
            "dodecahedron" => Some(Self::Dodecahedron),
            "icosahedron" => Some(Self::Icosahedron),
            _ => None,
        }
    }

    pub fn direction_count(self, fibonacci_resolution: u32) -> u32 {
        match self {
            Self::Fibonacci => fibonacci_resolution.max(1),
            Self::Tetrahedron => 4,
            Self::Cube => 6,
            Self::Octahedron => 8,
            Self::Dodecahedron => 12,
            Self::Icosahedron => 20,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub signaling_url: String,
    pub limits: LimitsConfig,
    pub dnve: DnveConfig,
    pub intervals: IntervalsConfig,
    pub webrtc: WebRtcConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LimitsConfig {
    pub max_connection_count: u32,
    pub expire_node_seconds: f32,
    pub hop_count: u32,
    pub reserved_connection_count: u32,
    pub force_disconnect_count: u32,
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
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WebRtcConfig {
    pub ice_servers: Vec<IceServer>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

impl Config {
    const AUTO_DIRECTION_THRESHOLD_SENTINEL: f32 = 0.0;

    pub fn new_default() -> Self {
        Self {
            signaling_url: "wss://rtc.tik-choco.com/signaling".to_string(),
            limits: LimitsConfig {
                max_connection_count: 30,
                expire_node_seconds: 10.0,
                hop_count: 2,
                reserved_connection_count: 1,
                force_disconnect_count: 0,
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
            },
            webrtc: WebRtcConfig {
                ice_servers: vec![IceServer {
                    urls: vec!["stun:stun.l.google.com:19302".to_string()],
                    username: None,
                    credential: None,
                }],
            },
        }
    }

    pub fn update_from_json(&mut self, json_str: &str) -> bool {
        if let Ok(new_config) = serde_json::from_str::<Config>(json_str) {
            *self = new_config;
            return true;
        }

        if let Ok(flat) = serde_json::from_str::<FlatConfig>(json_str) {
            flat.apply_to(self);
            return true;
        }

        false
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

/// Flat JSON format shared between `to_json_string` (serialise) and `update_from_json`
/// (partial deserialise).  Adding a new config knob requires editing only this struct
/// plus its two conversion methods — the compiler will catch any mismatch.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlatConfig {
    max_connection_count: Option<u32>,
    connection_balancer_interval_seconds: Option<f32>,
    expire_seconds: Option<f32>,
    aoi_range: Option<f32>,
    hop_count: Option<u32>,
    force_disconnect_count: Option<u32>,
    heartbeat_interval_seconds: Option<f32>,
    node_list_interval_seconds: Option<f32>,
    spatial_distance_layers: Option<u32>,
    spatial_density_resolution: Option<u32>,
    spatial_density_encoding: Option<String>,
    spatial_partition_type: Option<String>,
    direction_threshold: Option<f32>,
    connection_mode: Option<String>,
    node_list_exchange_mode: Option<String>,
}

impl FlatConfig {
    fn from_config(c: &Config) -> Self {
        Self {
            max_connection_count: Some(c.limits.max_connection_count),
            connection_balancer_interval_seconds: Some(c.intervals.connection_balancer),
            expire_seconds: Some(c.limits.expire_node_seconds),
            aoi_range: Some(c.dnve.aoi_range),
            hop_count: Some(c.limits.hop_count),
            force_disconnect_count: Some(c.limits.force_disconnect_count),
            heartbeat_interval_seconds: Some(c.intervals.heartbeat),
            node_list_interval_seconds: Some(c.intervals.node_list),
            spatial_distance_layers: Some(c.dnve.distance_layers),
            spatial_density_resolution: Some(c.dnve.density_resolution),
            spatial_density_encoding: Some(c.dnve.density_encoding.as_str().to_owned()),
            spatial_partition_type: Some(c.dnve.spatial_partition_type.as_str().to_owned()),
            direction_threshold: Some(c.dnve.direction_threshold),
            connection_mode: Some(c.dnve.connection_mode.as_str().to_owned()),
            node_list_exchange_mode: Some(c.dnve.node_list_exchange_mode.as_str().to_owned()),
        }
    }

    fn apply_to(self, c: &mut Config) {
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
        if let Some(v) = self.heartbeat_interval_seconds {
            c.intervals.heartbeat = v;
        }
        if let Some(v) = self.node_list_interval_seconds {
            c.intervals.node_list = v;
        }
        if let Some(v) = self.spatial_distance_layers {
            c.dnve.distance_layers = v;
        }
        if let Some(v) = self.spatial_density_resolution {
            c.dnve.density_resolution = v;
        }
        if let Some(v) = self.spatial_density_encoding {
            if let Some(parsed) = DensityEncoding::parse(&v) {
                c.dnve.density_encoding = parsed;
            }
        }
        if let Some(v) = self.spatial_partition_type {
            if let Some(parsed) = SpatialPartitionType::parse(&v) {
                c.dnve.spatial_partition_type = parsed;
            }
        }
        if let Some(v) = self.direction_threshold {
            c.dnve.direction_threshold = v;
        }
        if let Some(v) = self.connection_mode {
            if let Some(parsed) = ConnectionMode::parse(&v) {
                c.dnve.connection_mode = parsed;
            }
        }
        if let Some(v) = self.node_list_exchange_mode {
            if let Some(parsed) = NodeListExchangeMode::parse(&v) {
                c.dnve.node_list_exchange_mode = parsed;
            }
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, SpatialPartitionType};

    #[test]
    fn auto_direction_threshold_is_tighter_than_legacy_default_for_26_dirs() {
        let threshold = Config::effective_direction_threshold_for(26, 0.0);
        assert!(
            threshold > 0.7,
            "auto threshold should be tighter than 0.7, got {threshold}"
        );
        assert!(
            threshold < 1.0,
            "auto threshold must remain a valid cosine, got {threshold}"
        );
    }

    #[test]
    fn explicit_direction_threshold_override_is_preserved() {
        let threshold = Config::effective_direction_threshold_for(26, 0.82);
        assert!((threshold - 0.82).abs() < f32::EPSILON);
    }

    #[test]
    fn auto_direction_threshold_uses_partition_direction_count() {
        let mut config = Config::new_default();
        config.dnve.spatial_partition_type = SpatialPartitionType::Icosahedron;
        config.dnve.density_resolution = 6;

        assert_eq!(
            config.effective_direction_threshold(),
            Config::effective_direction_threshold_for(20, 0.0)
        );
    }
}
