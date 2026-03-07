use serde::{Deserialize, Serialize};
use std::f32::consts::PI;

const EPSILON: f32 = 0.00001;
const DEFAULT_MAX_RANGE: f32 = 1.0;
const EXPANSION_FACTOR: f32 = 0.5;
const BYTE_SCALE: f32 = 255.0;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct Vector3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vector3 {
    pub fn zero() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        }
    }

    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub fn dot(self, other: Self) -> f32 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    pub fn magnitude(self) -> f32 {
        self.sqr_magnitude().sqrt()
    }

    pub fn sqr_magnitude(self) -> f32 {
        self.x * self.x + self.y * self.y + self.z * self.z
    }

    pub fn normalized(self) -> Self {
        let mag = self.magnitude();
        if mag > 0.0 {
            Self {
                x: self.x / mag,
                y: self.y / mag,
                z: self.z / mag,
            }
        } else {
            Self::zero()
        }
    }

    pub fn dist(self, other: Self) -> f32 {
        (self - other).magnitude()
    }
}

impl std::ops::Sub for Vector3 {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self {
            x: self.x - other.x,
            y: self.y - other.y,
            z: self.z - other.z,
        }
    }
}

impl std::ops::Add for Vector3 {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self {
            x: self.x + other.x,
            y: self.y + other.y,
            z: self.z + other.z,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SpatialDensityData {
    pub density_map: Vec<f32>,
    pub position: Vector3,
    pub dir_count: usize,
    pub layer_count: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SpatialDensityDataByte {
    pub position: Vector3,
    pub max_value: f32,
    pub byte_densities: Vec<u8>,
    pub dir_count: usize,
    pub layer_count: usize,
}

impl SpatialDensityData {
    pub fn to_byte_encoded(&self) -> SpatialDensityDataByte {
        let max_value = self
            .density_map
            .iter()
            .copied()
            .fold(0.0_f32, |acc, v| acc.max(v));

        let byte_densities = if max_value <= EPSILON {
            vec![0; self.density_map.len()]
        } else {
            self.density_map
                .iter()
                .map(|&value| {
                    let normalized = (value / max_value).clamp(0.0, 1.0);
                    (normalized * BYTE_SCALE).round() as u8
                })
                .collect()
        };

        SpatialDensityDataByte {
            position: self.position,
            max_value,
            byte_densities,
            dir_count: self.dir_count,
            layer_count: self.layer_count,
        }
    }

    pub fn from_byte_encoded(encoded: &SpatialDensityDataByte) -> Self {
        let density_map = if encoded.max_value <= EPSILON {
            vec![0.0; encoded.byte_densities.len()]
        } else {
            encoded
                .byte_densities
                .iter()
                .map(|&value| (value as f32 / BYTE_SCALE) * encoded.max_value)
                .collect()
        };

        Self {
            density_map,
            position: encoded.position,
            dir_count: encoded.dir_count,
            layer_count: encoded.layer_count,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SpatialDensityUtils {
    pub directions: Vec<Vector3>,
}

impl SpatialDensityUtils {
    pub fn new(count: usize) -> Self {
        let directions = Self::generate_uniform_directions(count);
        Self { directions }
    }

    fn generate_uniform_directions(count: usize) -> Vec<Vector3> {
        let mut directions = Vec::with_capacity(count);
        if count == 1 {
            directions.push(Vector3::new(0.0, 0.0, 1.0));
            return directions;
        }

        let phi = PI * (3.0 - 5.0f32.sqrt());

        for i in 0..count {
            let y = 1.0 - (i as f32 / (count - 1) as f32) * 2.0;
            let radius = (1.0 - y * y).sqrt();
            let theta = phi * i as f32;

            let x = theta.cos() * radius;
            let z = theta.sin() * radius;

            directions.push(Vector3::new(x, y, z));
        }
        directions
    }

    pub fn create_spatial_density(
        &self,
        center: Vector3,
        nodes: &[Vector3],
        layer_count: usize,
        max_range: f32,
    ) -> SpatialDensityData {
        let dir_count = self.directions.len();
        let mut density_map = vec![0.0; dir_count * layer_count];

        let mut actual_max_range = max_range;
        if max_range <= EPSILON {
            actual_max_range = 0.0;
            for node in nodes {
                let d = center.dist(*node);
                if d > actual_max_range {
                    actual_max_range = d;
                }
            }
            if actual_max_range <= EPSILON {
                actual_max_range = DEFAULT_MAX_RANGE;
            }
        }

        for node in nodes {
            let vec = *node - center;
            let dist = vec.magnitude();
            let unit_vec = vec.normalized();
            let layer_index = ((dist / actual_max_range * layer_count as f32).floor() as usize)
                .min(layer_count - 1);

            for (i, dir) in self.directions.iter().enumerate() {
                let score = dir.dot(unit_vec);
                if score > 0.0 {
                    density_map[i * layer_count + layer_index] += score;
                }
            }
        }

        SpatialDensityData {
            density_map,
            position: center,
            dir_count,
            layer_count,
        }
    }

    pub fn project_spatial_density(
        &self,
        data: &SpatialDensityData,
        new_center: Vector3,
    ) -> SpatialDensityData {
        let offset = new_center - data.position;
        if offset.sqr_magnitude() <= EPSILON * EPSILON {
            return data.clone();
        }

        let offset_unit = offset.normalized();
        let mut projected_map = vec![0.0; data.density_map.len()];

        for i in 0..data.dir_count {
            let cos_sim = self.directions[i].dot(offset_unit).clamp(-1.0, 1.0);
            for j in 0..data.layer_count {
                let val =
                    data.density_map[i * data.layer_count + j] * (1.0 + EXPANSION_FACTOR * cos_sim);
                projected_map[i * data.layer_count + j] = val.max(0.0);
            }
        }

        SpatialDensityData {
            density_map: projected_map,
            position: new_center,
            dir_count: data.dir_count,
            layer_count: data.layer_count,
        }
    }

    pub fn merge_spatial_density(
        &self,
        self_data: &SpatialDensityData,
        other_data: &SpatialDensityData,
    ) -> SpatialDensityData {
        let other_projected = self.project_spatial_density(other_data, self_data.position);

        let mut merged_map = self_data.density_map.clone();
        for i in 0..merged_map.len() {
            merged_map[i] += other_projected.density_map[i];
        }

        SpatialDensityData {
            density_map: merged_map,
            position: self_data.position,
            dir_count: self_data.dir_count,
            layer_count: self_data.layer_count,
        }
    }
}
