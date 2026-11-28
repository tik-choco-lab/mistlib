use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use mistlib_core::overlay::node_store::NodeStore;
use mistlib_core::types::NodeId;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use web_time::Duration;

use super::{SessionCtx, ENGINE};

/// Steady cadence of the per-session overlay evaluation.
const OVERLAY_TICK_INTERVAL: Duration = Duration::from_millis(1000);

/// Minimum spacing between two early (node-learned) overlay runs. Bursts of
/// discoveries inside one window collapse into a single run, so early runs
/// never exceed 1 / `EARLY_TICK_MIN_SPACING` on top of the 1 s cadence.
const EARLY_TICK_MIN_SPACING: Duration = Duration::from_millis(100);

/// Wake signal for the overlay loop, raised when the session's node store
/// learns a node it didn't hold before. `pending` lets a burst of wakes that
/// arrive while a run is already scheduled collapse into that one run (the
/// leftover `Notify` permit then finds nothing pending and is skipped).
#[derive(Default)]
struct OverlayWake {
    pending: AtomicBool,
    notify: Notify,
}

impl OverlayWake {
    fn wake(&self) {
        self.pending.store(true, Ordering::Release);
        self.notify.notify_one();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayRun {
    /// The steady `OVERLAY_TICK_INTERVAL` tick.
    Periodic,
    /// Coalesced run after previously unknown nodes were learned.
    NodesLearned,
}

/// Drives one session's overlay evaluation: `run` is invoked on every
/// periodic tick (the first fires immediately) and, between ticks, once per
/// coalescing window after `wake`. Everything runs on this one task, so the
/// tick body stays single-threaded per session; an early run does not move
/// the periodic schedule.
async fn run_overlay_loop(
    cancel: CancellationToken,
    wake: Arc<OverlayWake>,
    period: Duration,
    early_spacing: Duration,
    mut run: impl FnMut(OverlayRun),
) {
    let mut interval = tokio::time::interval(period);
    let mut last_early: Option<tokio::time::Instant> = None;
    loop {
        let kind = tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => OverlayRun::Periodic,
            _ = wake.notify.notified() => OverlayRun::NodesLearned,
        };
        if kind == OverlayRun::NodesLearned {
            if let Some(last) = last_early {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep_until(last + early_spacing) => {}
                }
            }
            // Clear before running: nodes learned from here on are not seen by
            // this run and must schedule the next one.
            if !wake.pending.swap(false, Ordering::AcqRel) {
                continue;
            }
            last_early = Some(tokio::time::Instant::now());
        }
        run(kind);
    }
}

/// Wakes the overlay loop whenever `store` inserts a node it didn't hold,
/// except the local node's own entry (its position is not a discovery).
fn install_wake_listener(store: &mut NodeStore, wake: &Arc<OverlayWake>, local_id: NodeId) {
    let wake = wake.clone();
    store.set_new_node_listener(Arc::new(move |id| {
        if *id != local_id {
            wake.wake();
        }
    }));
}

impl super::MistEngine {
    /// Spawns this session's periodic AOI/neighbor and overlay-tick loops.
    /// Called exactly once, right after a session is inserted into the
    /// registry (see `layers/native_l0/room.rs::join_room`); cancelled via
    /// `ctx.cancel` when the room is left.
    pub fn spawn_background_loops(&self, ctx: Arc<SessionCtx>) {
        let cancel_aoi = ctx.cancel.clone();
        let ctx_aoi = ctx.clone();
        self.runtime.spawn(async move {
            let mut interval = tokio::time::interval(web_time::Duration::from_millis(1000));
            loop {
                tokio::select! {
                    _ = cancel_aoi.cancelled() => break,
                    _ = interval.tick() => {}
                }
                ctx_aoi.check_and_dispatch_aoi().await;
                ctx_aoi.check_and_dispatch_neighbors().await;
            }
        });

        // Any path that puts a new peer into the node store (signaling Data
        // via `ensure_node_registered`, DNVE3 heartbeat/position/node-list,
        // connection events) wakes the overlay loop. The local node's own
        // position entry is not a discovery.
        let wake = Arc::new(OverlayWake::default());
        let local_id = self.self_id.lock().unwrap().clone();
        install_wake_listener(&mut ctx.node_store.lock().unwrap(), &wake, local_id);

        let cancel_overlay = ctx.cancel.clone();
        self.runtime.spawn(run_overlay_loop(
            cancel_overlay,
            wake,
            OVERLAY_TICK_INTERVAL,
            EARLY_TICK_MIN_SPACING,
            move |kind| {
                let actions = if let (Some(ov), Some(wt)) = (&ctx.overlay, &ctx.link_selector) {
                    let states = wt.get_active_connection_states();
                    ov.sync_connection_states(&states);
                    let config = ENGINE.config.lock().unwrap().clone();
                    let mut actions = match kind {
                        OverlayRun::NodesLearned => ov.notify_nodes_learned(&config, &states),
                        OverlayRun::Periodic => Vec::new(),
                    };
                    actions.extend(ov.tick(&config, &states));
                    actions
                } else {
                    vec![]
                };

                for action in actions {
                    ENGINE.handle_action_for(ctx.clone(), action);
                }

                ENGINE.flush_expired_reorder(&ctx);
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mistlib_core::overlay::dnve3::Vector3;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Instant};

    // mistlib-native has no tokio `test-util`, so these run on real time.
    // Every assertion is one that scheduling lateness can't break: lateness
    // only delays runs (so spacing grows and counts shrink), and the
    // "prompt" bounds are far looser than the 1 s cadence they're compared to.

    /// Period long enough that only the immediate first tick is periodic.
    const NO_PERIODIC: Duration = Duration::from_secs(3600);

    fn spawn_loop(
        period: Duration,
    ) -> (
        Arc<OverlayWake>,
        mpsc::UnboundedReceiver<(OverlayRun, Instant)>,
        CancellationToken,
    ) {
        let wake = Arc::new(OverlayWake::default());
        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_overlay_loop(
            cancel.clone(),
            wake.clone(),
            period,
            EARLY_TICK_MIN_SPACING,
            move |kind| {
                let _ = tx.send((kind, Instant::now()));
            },
        ));
        (wake, rx, cancel)
    }

    async fn next_run(
        rx: &mut mpsc::UnboundedReceiver<(OverlayRun, Instant)>,
        within: Duration,
    ) -> Option<(OverlayRun, Instant)> {
        timeout(within, rx.recv()).await.ok().flatten()
    }

    #[tokio::test]
    async fn learned_node_triggers_evaluation_before_next_periodic_tick() {
        let (wake, mut rx, cancel) = spawn_loop(OVERLAY_TICK_INTERVAL);
        let (first, started) = next_run(&mut rx, Duration::from_secs(1)).await.unwrap();
        assert_eq!(first, OverlayRun::Periodic, "first tick fires immediately");

        tokio::time::sleep(Duration::from_millis(250)).await;
        let woke_at = Instant::now();
        wake.wake();

        // The next run must be the early one, not the 1 s periodic tick.
        let (kind, ran_at) = next_run(&mut rx, Duration::from_secs(2)).await.unwrap();
        assert_eq!(kind, OverlayRun::NodesLearned);
        assert!(
            ran_at - woke_at < Duration::from_millis(500),
            "early run took {:?} after the wake",
            ran_at - woke_at
        );

        // The periodic schedule is not moved by the early run. `started` is
        // taken inside the first run, slightly after the interval's origin,
        // so allow a small skew below the period.
        let (kind, ran_at) = next_run(&mut rx, Duration::from_secs(2)).await.unwrap();
        assert_eq!(kind, OverlayRun::Periodic);
        assert!(
            ran_at - started >= OVERLAY_TICK_INTERVAL - Duration::from_millis(50),
            "periodic tick ran early: {:?}",
            ran_at - started
        );
        cancel.cancel();
    }

    #[tokio::test]
    async fn discovery_burst_yields_bounded_early_runs() {
        let (wake, mut rx, cancel) = spawn_loop(NO_PERIODIC);
        assert_eq!(
            next_run(&mut rx, Duration::from_secs(1)).await.unwrap().0,
            OverlayRun::Periodic
        );

        // 50 discoveries, 5 ms apart.
        let burst_start = Instant::now();
        for _ in 0..50 {
            wake.wake();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let burst_len = burst_start.elapsed();

        let mut early = Vec::new();
        while let Some((kind, at)) = next_run(&mut rx, Duration::from_millis(400)).await {
            assert_eq!(kind, OverlayRun::NodesLearned);
            early.push(at);
        }
        cancel.cancel();

        // At most one run per 100 ms window over the burst, plus one trailing
        // run for wakes that landed after the last run started.
        let max_runs = (burst_len.as_millis() / EARLY_TICK_MIN_SPACING.as_millis()) as usize + 2;
        assert!(
            !early.is_empty() && early.len() <= max_runs,
            "{} early runs for a {burst_len:?} burst (max {max_runs})",
            early.len()
        );
        assert!(early.len() < 50, "burst was not coalesced");
        for pair in early.windows(2) {
            assert!(
                pair[1] - pair[0] >= EARLY_TICK_MIN_SPACING,
                "early runs {:?} apart, closer than the coalescing window",
                pair[1] - pair[0]
            );
        }
        assert!(
            early[0] - burst_start < Duration::from_millis(500),
            "first early run is prompt"
        );
    }

    #[tokio::test]
    async fn node_store_wakes_only_for_unknown_remote_nodes() {
        let (wake, mut rx, cancel) = spawn_loop(NO_PERIODIC);
        assert_eq!(
            next_run(&mut rx, Duration::from_secs(1)).await.unwrap().0,
            OverlayRun::Periodic
        );
        let local = NodeId("local".into());
        let mut store = NodeStore::new();
        install_wake_listener(&mut store, &wake, local.clone());

        store.update_node_position(local, Vector3::zero());
        assert!(
            next_run(&mut rx, Duration::from_millis(250))
                .await
                .is_none(),
            "own position is not a discovery"
        );

        let peer = NodeId("peer".into());
        let learned_at = Instant::now();
        store.update_node_position(peer.clone(), Vector3::zero());
        let (kind, ran_at) = next_run(&mut rx, Duration::from_secs(1)).await.unwrap();
        assert_eq!(kind, OverlayRun::NodesLearned);
        assert!(ran_at - learned_at < Duration::from_millis(500));

        // Updates to an already-known node don't wake the loop.
        store.update_node_position(peer.clone(), Vector3::new(1.0, 0.0, 0.0));
        assert!(store.touch_node(&peer));
        assert!(next_run(&mut rx, Duration::from_millis(250))
            .await
            .is_none());
        cancel.cancel();
    }

    #[tokio::test]
    async fn cancel_stops_a_pending_early_run() {
        let (wake, mut rx, cancel) = spawn_loop(NO_PERIODIC);
        next_run(&mut rx, Duration::from_secs(1)).await.unwrap();
        wake.wake();
        next_run(&mut rx, Duration::from_secs(1)).await.unwrap();
        // The next wake waits out the spacing window; cancel during it.
        wake.wake();
        cancel.cancel();
        assert!(next_run(&mut rx, Duration::from_millis(300))
            .await
            .is_none());
    }
}
