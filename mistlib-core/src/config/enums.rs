use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DensityEncoding {
    Float,
    #[default]
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

    pub fn variants() -> &'static [&'static str] {
        &["float", "byte"]
    }
}

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

    pub fn variants() -> &'static [&'static str] {
        &[
            "fibonacci",
            "tetrahedron",
            "cube",
            "octahedron",
            "dodecahedron",
            "icosahedron",
        ]
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

    pub fn variants() -> &'static [&'static str] {
        &[
            "direction_density",
            "direction_density_light",
            "node_list_directional",
            "node_list_aoi_guard",
            "node_list_aoi_proximity",
            "node_list_aoi_density",
            "node_list_proximity",
            "p_sense",
        ]
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NodeListExchangeMode {
    #[default]
    Pull,
    Push,
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

    pub fn variants() -> &'static [&'static str] {
        &["pull", "push"]
    }
}
