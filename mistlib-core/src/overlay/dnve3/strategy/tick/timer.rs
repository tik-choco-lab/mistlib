use crate::config::Config;
use rand::Rng;
use std::sync::Mutex;
use web_time::{Duration, Instant};

/// Applied uniformly to the heartbeat/node_list/balancer periodic timers so that
/// peers which start ticking in lockstep (e.g. everyone joining a room at once)
/// spread their sends out instead of bursting on the same wall-clock instant.
const PERIODIC_JITTER_RATIO: f32 = 0.2;

fn with_jitter(base_secs: f32) -> Duration {
    let base = base_secs.max(0.0);
    let mut rng = rand::thread_rng();
    let jitter = rng.gen_range(0.0..=(base * PERIODIC_JITTER_RATIO));
    Duration::from_secs_f32(base + jitter)
}

pub(super) fn heartbeat_interval_with_jitter(config: &Config) -> Duration {
    with_jitter(config.intervals.heartbeat)
}

pub(super) fn node_list_interval_with_jitter(config: &Config) -> Duration {
    with_jitter(config.intervals.node_list)
}

pub(super) fn balancer_interval_with_jitter(config: &Config) -> Duration {
    with_jitter(config.intervals.connection_balancer)
}

/// PING keepalive intentionally has no jitter: liveness/miss detection reasons
/// about "did the previous round get answered", which is easiest to keep sound
/// with a steady cadence.
pub(super) fn ping_interval(config: &Config) -> Duration {
    Duration::from_secs_f32(config.intervals.ping.max(0.0))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> Config {
        let mut c = Config::new_default();
        c.intervals.heartbeat = 1.0;
        c.intervals.node_list = 2.0;
        c.intervals.connection_balancer = 4.0;
        c.intervals.ping = 1.0;
        c
    }

    #[test]
    fn jittered_intervals_stay_within_the_configured_ratio() {
        let config = base_config();
        for _ in 0..200 {
            let hb = heartbeat_interval_with_jitter(&config).as_secs_f32();
            assert!(
                (1.0..=1.2 + 1e-4).contains(&hb),
                "heartbeat jitter out of range: {hb}"
            );

            let nl = node_list_interval_with_jitter(&config).as_secs_f32();
            assert!(
                (2.0..=2.4 + 1e-4).contains(&nl),
                "node_list jitter out of range: {nl}"
            );

            let bal = balancer_interval_with_jitter(&config).as_secs_f32();
            assert!(
                (4.0..=4.8 + 1e-4).contains(&bal),
                "balancer jitter out of range: {bal}"
            );
        }
    }

    #[test]
    fn ping_interval_has_no_jitter() {
        let config = base_config();
        for _ in 0..20 {
            assert_eq!(ping_interval(&config).as_secs_f32(), 1.0);
        }
    }

    #[test]
    fn zero_base_interval_has_no_jitter_and_is_always_due() {
        let mut config = base_config();
        config.intervals.node_list = 0.0;
        for _ in 0..20 {
            assert_eq!(node_list_interval_with_jitter(&config), Duration::ZERO);
        }
    }
}
