use crate::config::Config;
use rand::Rng;
use std::sync::Mutex;
use web_time::{Duration, Instant};

const BALANCER_JITTER_RATIO: f32 = 0.2;

pub(super) fn heartbeat_interval(config: &Config) -> Duration {
    Duration::from_secs_f32(config.intervals.heartbeat.max(0.0))
}

pub(super) fn node_list_interval(config: &Config) -> Duration {
    Duration::from_secs_f32(config.intervals.node_list.max(0.0))
}

pub(super) fn balancer_interval_with_jitter(config: &Config) -> Duration {
    let base = config.intervals.connection_balancer.max(0.0);
    let mut rng = rand::thread_rng();
    let jitter = rng.gen_range(0.0..=(base * BALANCER_JITTER_RATIO));
    Duration::from_secs_f32(base + jitter)
}

pub(super) fn is_due(
    timer: &Mutex<Option<Instant>>,
    name: &'static str,
    now: Instant,
    initial_due_at: Instant,
    next_due_at: Instant,
) -> bool {
    let mut due_lock = timer
        .lock()
        .unwrap_or_else(|_| panic!("{name} lock poisoned"));
    let due_at = due_lock.get_or_insert(initial_due_at);
    if now < *due_at {
        return false;
    }

    *due_at = next_due_at;
    true
}
