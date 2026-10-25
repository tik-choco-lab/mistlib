use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

/// Activity belongs to one Peer generation and covers all inbound transport bytes.
/// The receive callback only reads the monotonic clock and updates an atomic.
#[derive(Debug)]
pub(crate) struct InboundActivity {
    epoch: Instant,
    transport_rx: Mutex<mistlib_core::stats::TransportReceiveCounter>,
    // Nanoseconds since epoch, plus one; zero means no frame received yet.
    last_rx: AtomicU64,
    extension_logged_for: Mutex<Option<Instant>>,
}

impl InboundActivity {
    pub(crate) fn new() -> Self {
        Self {
            epoch: Instant::now(),
            transport_rx: Mutex::new(Default::default()),
            last_rx: AtomicU64::new(0),
            extension_logged_for: Mutex::new(None),
        }
    }

    /// SCTP repair/DTLS traffic is proof of path and remote-stack liveness even
    /// when no application message can complete. This intentionally detects an
    /// alive stack with a hung application later; ICE failure/consent still wins.
    pub(crate) fn record_transport_bytes(&self, report_id: &str, bytes: u64) -> bool {
        let grew = self
            .transport_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .observe(report_id, bytes);
        if grew {
            self.record();
        }
        grew
    }

    pub(crate) fn record(&self) {
        let timestamp = (self.epoch.elapsed().as_nanos() as u64).saturating_add(1);
        // Callbacks on different channels can race. Never move last-rx back.
        self.last_rx.fetch_max(timestamp, Ordering::Relaxed);
    }

    pub(crate) fn last_rx(&self) -> Option<Instant> {
        let timestamp = self.last_rx.load(Ordering::Relaxed);
        timestamp
            .checked_sub(1)
            .map(|ns| self.epoch + Duration::from_nanos(ns))
    }

    /// Called only by the sweeper, once it observes an extension by traffic.
    pub(crate) fn log_extension_once(&self, grace_start: Instant) -> bool {
        let mut logged = self
            .extension_logged_for
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *logged == Some(grace_start) {
            return false;
        }
        *logged = Some(grace_start);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_and_extension_log_are_scoped_to_peer_and_grace() {
        let activity = InboundActivity::new();
        let other_peer = InboundActivity::new();
        assert_eq!(activity.last_rx(), None);
        activity.record();
        let first_rx = activity.last_rx().unwrap();
        activity.record();
        assert!(activity.last_rx().unwrap() >= first_rx);
        assert_eq!(other_peer.last_rx(), None);

        let start = Instant::now();
        assert!(activity.log_extension_once(start));
        assert!(!activity.log_extension_once(start));
        assert!(activity.log_extension_once(start + Duration::from_secs(1)));
    }
}
