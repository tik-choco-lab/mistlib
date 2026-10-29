use mistlib_core::signaling::reconnect::ReconnectBackoff;
use std::collections::HashMap;
use web_time::Instant;

#[derive(Default)]
pub(super) struct SocketState {
    opened: bool,
}

impl SocketState {
    pub(super) fn on_open(&mut self) {
        self.opened = true;
    }

    pub(super) fn on_close(&mut self) -> bool {
        std::mem::take(&mut self.opened)
    }
}

#[derive(Default)]
pub(super) struct ReconnectState {
    epoch: u64,
    running: HashMap<String, u64>,
    backoffs: HashMap<String, ReconnectBackoff>,
}

impl ReconnectState {
    pub(super) fn reset(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.running.clear();
        self.backoffs.clear();
    }

    pub(super) fn claim(&mut self, relay: &str, epoch: u64) -> bool {
        if self.epoch != epoch || self.running.contains_key(relay) {
            return false;
        }
        self.running.insert(relay.to_owned(), epoch);
        true
    }

    pub(super) fn release(&mut self, relay: &str, epoch: u64) {
        if self.running.get(relay) == Some(&epoch) {
            self.running.remove(relay);
        }
    }

    pub(super) fn backoff(&mut self, relay: &str) -> &mut ReconnectBackoff {
        self.backoffs.entry(relay.to_owned()).or_default()
    }

    pub(super) fn on_connected(&mut self, relay: &str, now: Instant) {
        self.backoff(relay).on_connected(now);
    }

    pub(super) fn on_disconnected(&mut self, relay: &str, now: Instant) {
        self.backoff(relay).on_disconnected(now);
    }
}
