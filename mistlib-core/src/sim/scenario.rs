use super::{LinkCondition, SimNetwork};
use crate::error::Result;
use crate::transport::{NetworkEvent, NetworkEventHandler, Transport};
use crate::types::{DeliveryMethod, NodeId};
use bytes::Bytes;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::{create_dir_all, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

const POSITION_MSG_TYPE: &str = "update_position";
const DEFAULT_BOUNDS: Vector3 = Vector3::new(100.0, 100.0, 100.0);
const DEFAULT_SPEED_MIN: f64 = 1.0;
const DEFAULT_SPEED_MAX: f64 = 5.0;
const DEFAULT_TAU_DIR: f64 = 1.0;

#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    pub nodes: usize,
    pub seed: u64,
    pub duration: f64,
    pub tick: f64,
    pub aoi_radius: f64,
    pub latency: Duration,
    pub jitter: Duration,
    pub loss: f64,
    pub out_dir: PathBuf,
    pub bounds: [f64; 3],
    pub speed_min: f64,
    pub speed_max: f64,
    pub tau_dir: f64,
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            nodes: 5,
            seed: 1,
            duration: 1.0,
            tick: 0.1,
            aoi_radius: 10.0,
            latency: Duration::ZERO,
            jitter: Duration::ZERO,
            loss: 0.0,
            out_dir: PathBuf::from("sim-out"),
            bounds: DEFAULT_BOUNDS.into(),
            speed_min: DEFAULT_SPEED_MIN,
            speed_max: DEFAULT_SPEED_MAX,
            tau_dir: DEFAULT_TAU_DIR,
        }
    }
}

pub async fn run_scenario(config: ScenarioConfig) -> Result<()> {
    config.validate()?;
    create_dir_all(&config.out_dir).map_err(io_error)?;

    let logger = Arc::new(JsonlLogger::new(config.out_dir.clone())?);
    let started = Instant::now();
    let mut rng = StdRng::seed_from_u64(config.seed);
    let bounds = Vector3::from(config.bounds);

    let network = Arc::new(SimNetwork::new(config.seed));
    network.set_default_condition(LinkCondition::new(
        config.latency,
        config.jitter,
        config.loss,
    ));

    let mut nodes = Vec::with_capacity(config.nodes);
    let mut transports = Vec::with_capacity(config.nodes);

    for id in 0..config.nodes {
        let node = Arc::new(ScenarioNode::new(
            id,
            random_point(bounds, &mut rng),
            random_speed(&config, &mut rng),
        ));
        let transport = Arc::new(network.transport(node_id(id)));
        transport
            .start(Arc::new(ScenarioHandler {
                node: Arc::clone(&node),
                logger: Arc::clone(&logger),
                started,
            }))
            .await?;
        nodes.push(node);
        transports.push(transport);
    }

    for src in 0..config.nodes {
        for dst in 0..config.nodes {
            if src != dst {
                transports[src].connect(&node_id(dst)).await?;
            }
        }
    }

    let mut t = 0.0;
    logger.log_positions(t, &nodes)?;
    logger.log_estimated(t, &nodes)?;

    while t + f64::EPSILON < config.duration {
        for node in &nodes {
            node.step_random_walk(config.tick, bounds, config.tau_dir, &mut rng);
        }

        for src in 0..config.nodes {
            let update = nodes[src].position_update(t);
            let payload = Bytes::from(serde_json::to_vec(&update)?);
            for dst in 0..config.nodes {
                if src == dst {
                    continue;
                }
                logger.log_event(json!({
                    "t": round_time(started.elapsed().as_secs_f64()),
                    "sim_t": round_time(t),
                    "event": "send",
                    "src": src,
                    "dst": dst,
                    "msg_type": POSITION_MSG_TYPE,
                    "size_bytes": payload.len(),
                }))?;
                transports[src]
                    .send(&node_id(dst), payload.clone(), DeliveryMethod::Unreliable)
                    .await?;
            }
        }

        tokio::time::sleep(Duration::from_secs_f64(config.tick)).await;
        t = (t + config.tick).min(config.duration);
        logger.log_positions(t, &nodes)?;
        logger.log_estimated(t, &nodes)?;
    }

    let drain = config.latency.saturating_add(config.jitter);
    if !drain.is_zero() {
        tokio::time::sleep(drain).await;
    } else {
        tokio::task::yield_now().await;
    }
    logger.flush()?;
    Ok(())
}

impl ScenarioConfig {
    fn validate(&self) -> Result<()> {
        if self.nodes == 0 {
            return Err(crate::MistError::Internal(
                "scenario requires at least one node".into(),
            ));
        }
        if self.tick <= 0.0 || !self.tick.is_finite() {
            return Err(crate::MistError::Internal(
                "scenario tick must be positive".into(),
            ));
        }
        if self.duration < 0.0 || !self.duration.is_finite() {
            return Err(crate::MistError::Internal(
                "scenario duration must be finite and non-negative".into(),
            ));
        }
        if self.tau_dir <= 0.0 || !self.tau_dir.is_finite() {
            return Err(crate::MistError::Internal(
                "scenario tau_dir must be positive".into(),
            ));
        }
        if self.speed_min < 0.0 || self.speed_max < self.speed_min {
            return Err(crate::MistError::Internal(
                "scenario speed range is invalid".into(),
            ));
        }
        if self.bounds.iter().any(|v| *v <= 0.0 || !v.is_finite()) {
            return Err(crate::MistError::Internal(
                "scenario bounds must be positive".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Vector3 {
    x: f64,
    y: f64,
    z: f64,
}

impl Vector3 {
    const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }
}

impl From<[f64; 3]> for Vector3 {
    fn from(value: [f64; 3]) -> Self {
        Self::new(value[0], value[1], value[2])
    }
}

impl From<Vector3> for [f64; 3] {
    fn from(value: Vector3) -> Self {
        [value.x, value.y, value.z]
    }
}

#[derive(Debug, Clone, Copy)]
struct EstimatedPosition {
    position: Vector3,
    recv_time: f64,
}

struct ScenarioNode {
    id: usize,
    position: Mutex<Vector3>,
    speed: f64,
    walk_direction: Mutex<Vector3>,
    walk_direction_remaining: Mutex<f64>,
    estimated: Mutex<BTreeMap<usize, EstimatedPosition>>,
}

impl ScenarioNode {
    fn new(id: usize, position: Vector3, speed: f64) -> Self {
        Self {
            id,
            position: Mutex::new(position),
            speed,
            walk_direction: Mutex::new(Vector3::new(1.0, 0.0, 0.0)),
            walk_direction_remaining: Mutex::new(0.0),
            estimated: Mutex::new(BTreeMap::new()),
        }
    }

    fn step_random_walk(&self, dt: f64, bounds: Vector3, tau_dir: f64, rng: &mut StdRng) {
        let mut remaining_dt = dt;
        while remaining_dt > 1e-12 {
            let mut dir_remaining = self.walk_direction_remaining.lock().unwrap();
            if *dir_remaining <= 1e-12 {
                *self.walk_direction.lock().unwrap() = random_direction(rng);
                *dir_remaining = tau_dir;
            }

            let segment_dt = remaining_dt.min(*dir_remaining);
            let dir = *self.walk_direction.lock().unwrap();
            let mut pos = self.position.lock().unwrap();
            *pos = torus_wrap(
                Vector3::new(
                    pos.x + self.speed * dir.x * segment_dt,
                    pos.y + self.speed * dir.y * segment_dt,
                    pos.z + self.speed * dir.z * segment_dt,
                ),
                bounds,
            );
            remaining_dt -= segment_dt;
            *dir_remaining -= segment_dt;
        }
    }

    fn position_update(&self, generated_t: f64) -> PositionUpdate {
        PositionUpdate {
            node_id: self.id,
            generated_t,
            position: *self.position.lock().unwrap(),
        }
    }
}

struct ScenarioHandler {
    node: Arc<ScenarioNode>,
    logger: Arc<JsonlLogger>,
    started: Instant,
}

impl NetworkEventHandler for ScenarioHandler {
    fn on_event(&self, event: NetworkEvent) {
        let Ok(update) = serde_json::from_slice::<PositionUpdate>(&event.data) else {
            return;
        };
        let Some(src) = parse_node_id(&event.from) else {
            return;
        };
        let recv_t = self.started.elapsed().as_secs_f64();
        self.node.estimated.lock().unwrap().insert(
            update.node_id,
            EstimatedPosition {
                position: update.position,
                recv_time: recv_t,
            },
        );
        let _ = self.logger.log_event(json!({
            "t": round_time(recv_t),
            "sim_t": round_time(update.generated_t),
            "event": "recv",
            "src": src,
            "dst": self.node.id,
            "msg_type": POSITION_MSG_TYPE,
        }));
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PositionUpdate {
    node_id: usize,
    generated_t: f64,
    position: Vector3,
}

struct JsonlLogger {
    positions: Mutex<BufWriter<File>>,
    estimated: Mutex<BufWriter<File>>,
    events: Mutex<BufWriter<File>>,
}

impl JsonlLogger {
    fn new(out_dir: PathBuf) -> Result<Self> {
        Ok(Self {
            positions: Mutex::new(BufWriter::new(
                File::create(out_dir.join("positions.jsonl")).map_err(io_error)?,
            )),
            estimated: Mutex::new(BufWriter::new(
                File::create(out_dir.join("estimated.jsonl")).map_err(io_error)?,
            )),
            events: Mutex::new(BufWriter::new(
                File::create(out_dir.join("events.jsonl")).map_err(io_error)?,
            )),
        })
    }

    fn log_positions(&self, t: f64, nodes: &[Arc<ScenarioNode>]) -> Result<()> {
        let rows: Vec<_> = nodes
            .iter()
            .map(|node| {
                let pos = *node.position.lock().unwrap();
                json!([
                    node.id,
                    round_coord(pos.x),
                    round_coord(pos.y),
                    round_coord(pos.z)
                ])
            })
            .collect();
        self.write_line(
            &self.positions,
            json!({ "t": round_time(t), "nodes": rows }),
        )
    }

    fn log_estimated(&self, t: f64, nodes: &[Arc<ScenarioNode>]) -> Result<()> {
        let rows: Vec<_> = nodes
            .iter()
            .map(|node| {
                let estimates: Vec<_> = node
                    .estimated
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(peer, estimate)| {
                        json!([
                            peer,
                            round_coord(estimate.position.x),
                            round_coord(estimate.position.y),
                            round_coord(estimate.position.z),
                            round_time(estimate.recv_time),
                        ])
                    })
                    .collect();
                json!([node.id, estimates])
            })
            .collect();
        self.write_line(
            &self.estimated,
            json!({ "t": round_time(t), "nodes": rows }),
        )
    }

    fn log_event(&self, value: serde_json::Value) -> Result<()> {
        self.write_line(&self.events, value)
    }

    fn write_line(&self, writer: &Mutex<BufWriter<File>>, value: serde_json::Value) -> Result<()> {
        let mut writer = writer.lock().unwrap();
        serde_json::to_writer(&mut *writer, &value)?;
        writer.write_all(b"\n").map_err(io_error)
    }

    fn flush(&self) -> Result<()> {
        self.positions.lock().unwrap().flush().map_err(io_error)?;
        self.estimated.lock().unwrap().flush().map_err(io_error)?;
        self.events.lock().unwrap().flush().map_err(io_error)
    }
}

fn node_id(id: usize) -> NodeId {
    NodeId(id.to_string())
}

fn parse_node_id(id: &NodeId) -> Option<usize> {
    id.0.parse().ok()
}

fn random_point(bounds: Vector3, rng: &mut StdRng) -> Vector3 {
    Vector3::new(
        rng.gen_range(0.0..bounds.x),
        rng.gen_range(0.0..bounds.y),
        rng.gen_range(0.0..bounds.z),
    )
}

fn random_speed(config: &ScenarioConfig, rng: &mut StdRng) -> f64 {
    if (config.speed_max - config.speed_min).abs() <= f64::EPSILON {
        config.speed_min
    } else {
        rng.gen_range(config.speed_min..=config.speed_max)
    }
}

fn random_direction(rng: &mut StdRng) -> Vector3 {
    let theta = rng.gen_range(0.0..(2.0 * std::f64::consts::PI));
    let phi = rng.gen_range(-1.0_f64..=1.0).acos();
    Vector3::new(phi.sin() * theta.cos(), phi.sin() * theta.sin(), phi.cos())
}

fn torus_wrap(pos: Vector3, bounds: Vector3) -> Vector3 {
    Vector3::new(
        pos.x.rem_euclid(bounds.x),
        pos.y.rem_euclid(bounds.y),
        pos.z.rem_euclid(bounds.z),
    )
}

fn round_time(value: f64) -> f64 {
    (value * 1_000_000_000.0).round() / 1_000_000_000.0
}

fn round_coord(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn io_error(error: std::io::Error) -> crate::MistError {
    crate::MistError::Other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs::{read_to_string, remove_dir_all};

    #[tokio::test]
    async fn small_run_writes_dnve7_compatible_jsonl_files() {
        let out_dir =
            std::env::temp_dir().join(format!("mistlib-sim-run-{}-{}", std::process::id(), 7));
        let _ = remove_dir_all(&out_dir);

        run_scenario(ScenarioConfig {
            nodes: 5,
            seed: 7,
            duration: 0.02,
            tick: 0.01,
            aoi_radius: 10.0,
            latency: Duration::ZERO,
            jitter: Duration::ZERO,
            loss: 0.0,
            out_dir: out_dir.clone(),
            ..ScenarioConfig::default()
        })
        .await
        .unwrap();

        let positions = read_jsonl(out_dir.join("positions.jsonl"));
        let estimated = read_jsonl(out_dir.join("estimated.jsonl"));
        let events = read_jsonl(out_dir.join("events.jsonl"));

        assert!(positions.len() >= 2);
        assert_eq!(positions.len(), estimated.len());
        assert!(!events.is_empty());

        for frame in positions {
            assert!(frame.get("t").unwrap().is_number());
            let nodes = frame.get("nodes").unwrap().as_array().unwrap();
            assert_eq!(nodes.len(), 5);
            for row in nodes {
                let row = row.as_array().unwrap();
                assert_eq!(row.len(), 4);
                assert!(row[0].as_u64().is_some());
                assert!(row[1].is_number());
                assert!(row[2].is_number());
                assert!(row[3].is_number());
            }
        }

        for frame in estimated {
            assert!(frame.get("t").unwrap().is_number());
            for row in frame.get("nodes").unwrap().as_array().unwrap() {
                let row = row.as_array().unwrap();
                assert_eq!(row.len(), 2);
                assert!(row[0].as_u64().is_some());
                for est in row[1].as_array().unwrap() {
                    let est = est.as_array().unwrap();
                    assert_eq!(est.len(), 5);
                    assert!(est[0].as_u64().is_some());
                    assert!(est[1].is_number());
                    assert!(est[2].is_number());
                    assert!(est[3].is_number());
                    assert!(est[4].is_number());
                }
            }
        }

        for event in events {
            assert!(event.get("t").unwrap().is_number());
            assert!(matches!(
                event.get("event").and_then(Value::as_str),
                Some("send" | "recv")
            ));
            assert!(event.get("src").unwrap().as_u64().is_some());
            assert!(event.get("dst").unwrap().as_u64().is_some());
            assert_eq!(
                event.get("msg_type").and_then(Value::as_str),
                Some(POSITION_MSG_TYPE)
            );
        }

        let _ = remove_dir_all(out_dir);
    }

    fn read_jsonl(path: PathBuf) -> Vec<Value> {
        read_to_string(path)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
