//! The optional channel is link-local; capability state belongs to one PC.
use std::sync::atomic::{AtomicBool, Ordering};

pub const RELIABLE_UNORDERED_LABEL: &str = "reliable-unordered";

#[derive(Default)]
pub struct ReliableUnorderedCapability {
    local: AtomicBool,
    remote: AtomicBool,
    initial_offerer: AtomicBool,
    attempted: AtomicBool,
    disabled: AtomicBool,
}

impl ReliableUnorderedCapability {
    pub fn set_local(&self, enabled: bool) {
        self.local.store(enabled, Ordering::SeqCst);
    }
    pub fn local(&self) -> bool {
        self.local.load(Ordering::SeqCst)
    }
    pub fn accept_remote(&self, advertised: Option<u8>) {
        // Only called after accepting a description for this PC. Absence on
        // later legacy renegotiation does not manufacture new support.
        if advertised == Some(1) {
            self.remote.store(true, Ordering::SeqCst);
        }
    }
    pub fn set_initial_offerer(&self) {
        self.initial_offerer.store(true, Ordering::SeqCst);
    }
    pub fn accepts_channel(&self) -> bool {
        self.local() && self.remote.load(Ordering::SeqCst) && !self.disabled.load(Ordering::SeqCst)
    }
    pub fn begin_open(&self) -> bool {
        self.accepts_channel()
            && self.initial_offerer.load(Ordering::SeqCst)
            && !self.attempted.swap(true, Ordering::SeqCst)
    }
    pub fn disable(&self) {
        self.disabled.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChannelLabel {
    Reliable,
    UnreliableOrdered,
    Unreliable,
    ReliableUnordered,
}

pub fn channel_label(label: &str) -> Option<ChannelLabel> {
    match label {
        "reliable" => Some(ChannelLabel::Reliable),
        "unreliable-ordered" => Some(ChannelLabel::UnreliableOrdered),
        "unreliable" => Some(ChannelLabel::Unreliable),
        RELIABLE_UNORDERED_LABEL => Some(ChannelLabel::ReliableUnordered),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_mutually_capable_initial_offerer_opens_once_per_pc_generation() {
        let pc = ReliableUnorderedCapability::default();
        pc.set_local(true);
        pc.set_initial_offerer();
        assert!(!pc.begin_open());
        pc.accept_remote(Some(2));
        assert!(!pc.begin_open());
        pc.accept_remote(Some(1));
        assert!(pc.begin_open());
        assert!(!pc.begin_open());
        pc.disable();
        assert!(!pc.accepts_channel());
        let replacement = ReliableUnorderedCapability::default();
        replacement.set_local(true);
        assert!(!replacement.accepts_channel());
        replacement.accept_remote(Some(1));
        assert!(replacement.accepts_channel());
        assert!(
            !replacement.begin_open(),
            "answerer never creates a duplicate channel"
        );
        assert_eq!(channel_label("future-unknown"), None);
    }
}
