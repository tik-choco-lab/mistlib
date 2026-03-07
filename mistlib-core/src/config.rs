use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DensityEncoding {
    Float,
    Byte,
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

impl Default for DensityEncoding {
    fn default() -> Self {
        Self::Byte
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
    pub expire_node_seconds: u32,
    pub hop_count: u32,
    pub reserved_connection_count: u32,
    pub force_disconnect_count: u32,
    pub exchange_count: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DnveConfig {
    pub density_max_range: f32,
    pub distance_layers: u32,
    pub density_resolution: u32,
    #[serde(default)]
    pub density_encoding: DensityEncoding,
    pub direction_threshold: f32,
    pub aoi_range: f32,
    pub nearest_node_count: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IntervalsConfig {
    pub connection_balancer: f32,
    pub visible_nodes: f32,
    pub heartbeat: f32,
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
    pub fn new_default() -> Self {
        Self {
            signaling_url: "wss://rtc.tik-choco.com/signaling".to_string(),
            limits: LimitsConfig {
                max_connection_count: 30,
                expire_node_seconds: 4,
                hop_count: 3,
                reserved_connection_count: 1,
                force_disconnect_count: 3,
                exchange_count: 3,
            },
            dnve: DnveConfig {
                density_max_range: 64.0,
                distance_layers: 4,
                density_resolution: 26,
                density_encoding: DensityEncoding::Byte,
                direction_threshold: 0.7,
                aoi_range: 64.0,
                nearest_node_count: 5,
            },
            intervals: IntervalsConfig {
                connection_balancer: 1.0,
                visible_nodes: 1.0,
                heartbeat: 1.0,
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

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PartialConfig {
            max_connection_count: Option<u32>,
            aoi_range: Option<f32>,
            direction_threshold: Option<f32>,
            density_resolution: Option<u32>,
            density_encoding: Option<String>,
        }

        if let Ok(partial) = serde_json::from_str::<PartialConfig>(json_str) {
            if let Some(max) = partial.max_connection_count {
                self.limits.max_connection_count = max;
            }
            if let Some(aoi) = partial.aoi_range {
                self.dnve.aoi_range = aoi;
            }
            if let Some(threshold) = partial.direction_threshold {
                self.dnve.direction_threshold = threshold;
            }
            if let Some(resolution) = partial.density_resolution {
                self.dnve.density_resolution = resolution;
            }
            if let Some(encoding) = partial.density_encoding {
                if let Some(parsed) = DensityEncoding::parse(&encoding) {
                    self.dnve.density_encoding = parsed;
                }
            }
            return true;
        }

        false
    }

    pub fn to_json_string(&self) -> String {
        let opt_config = serde_json::json!({
            "visibleCount": self.dnve.nearest_node_count,
            "maxConnectionCount": self.limits.max_connection_count,
            "connectionBalancerIntervalSeconds": self.intervals.connection_balancer,
            "visibleNodesIntervalSeconds": self.intervals.visible_nodes,
            "expireSeconds": self.limits.expire_node_seconds as f32,
            "aoiRange": self.dnve.aoi_range,
            "hopCount": self.limits.hop_count,
            "forceDisconnectCount": self.limits.force_disconnect_count,
            "heartbeatIntervalSeconds": self.intervals.heartbeat,
            "exchangeCount": self.limits.exchange_count,
            "spatialDistanceLayers": self.dnve.distance_layers,
            "spatialDensityResolution": self.dnve.density_resolution,
            "spatialDensityEncoding": self.dnve.density_encoding.as_str()
        });

        serde_json::to_string(&opt_config).unwrap_or_else(|_| "{}".to_string())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new_default()
    }
}
