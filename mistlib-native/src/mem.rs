use std::sync::atomic::{AtomicU64, Ordering};

static LAST_RSS_KB: AtomicU64 = AtomicU64::new(0);
/// Peers inserted into the HashMap since the last tick.
static INSERT_COUNT: AtomicU64 = AtomicU64::new(0);
/// Peers removed from the HashMap since the last tick.
static CLEANUP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Returns the process RSS (Resident Set Size) in kilobytes.
/// Reads /proc/self/status on Linux; returns 0 on other platforms or on read error.
pub fn rss_kb() -> u64 {
    #[cfg(target_os = "linux")]
    {
        use std::io::{BufRead, BufReader};
        if let Ok(f) = std::fs::File::open("/proc/self/status") {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    return rest
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                }
            }
        }
    }
    0
}

/// Call this each time a peer is inserted into the `peers` HashMap.
pub fn record_peer_inserted() {
    INSERT_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Call this each time a peer is removed from the `peers` HashMap (any code path).
pub fn record_peer_cleaned() {
    CLEANUP_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Emit one aggregate log line. Resets the insert/cleanup counters and the RSS baseline.
///
/// Format:
///   [MEM] tick peers=N states=N pending=N last_disc=N rss_kb=N delta_kb=±N inserts=N cleanups=N
///
/// Interpretation guide:
/// - `delta_kb` growing while `inserts ≈ cleanups`  → leak inside RTCPeerConnection (not the HashMap)
/// - `last_disc` growing indefinitely               → last_disconnect_at never pruned
/// - `pending` not returning to 0                  → ICE candidate accumulation
pub fn log_mem_tick(peers: usize, states: usize, pending: usize, last_disc: usize) {
    let current = rss_kb();
    let last = LAST_RSS_KB.swap(current, Ordering::Relaxed);
    let delta = current as i64 - last as i64;
    let inserts: u64 = INSERT_COUNT.swap(0, Ordering::Relaxed);
    let cleanups: u64 = CLEANUP_COUNT.swap(0, Ordering::Relaxed);

    let msg = format!(
        "[MEM] tick peers={} states={} pending={} last_disc={} rss_kb={} delta_kb={:+} inserts={} cleanups={}",
        peers, states, pending, last_disc, current, delta, inserts, cleanups
    );
    crate::logging::dispatch_log(crate::logging::LOG_INFO, &msg);
}
