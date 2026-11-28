//! Observations only: no result from this module controls signaling.
//! The caller supplies time, so native and browsers share retention semantics.
use super::NostrEvent;
use crate::signaling::SignalingType;
use std::collections::{BTreeMap, HashMap};
use web_time::{Duration, Instant};

const CAPACITY: usize = 2048;
const TTL: Duration = Duration::from_secs(120);
const TARGET_TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Debug)]
pub struct PublishMetadata {
    pub kind: u32,
    pub label: String,
    pub target: String,
    pub relays: Vec<String>,
}

#[derive(Default, Debug)]
struct Counts {
    publish: u64,
    accepted: u64,
    rejected: u64,
}

#[derive(Default)]
pub struct NostrMeasurements {
    events: HashMap<String, (Instant, PublishMetadata)>,
    counts: BTreeMap<String, Counts>,
    targets: HashMap<String, (Instant, bool)>,
    last_report: Option<Instant>,
    req: u64,
    resets: u64,
    received: u64,
}

pub fn signaling_label(kind: &SignalingType) -> &'static str {
    match kind {
        SignalingType::Offer => "offer",
        SignalingType::Answer => "answer",
        SignalingType::Candidate => "candidate",
        SignalingType::Candidates => "candidates",
        SignalingType::Request => "request",
        SignalingType::CandidateAck => "candidate_ack",
        SignalingType::NegotiationAck => "negotiation_ack",
        SignalingType::Rejoin => "rejoin",
        SignalingType::IceQuicOffer => "ice_quic_offer",
        SignalingType::IceQuicAnswer => "ice_quic_answer",
    }
}

impl NostrMeasurements {
    fn prune(&mut self, now: Instant) {
        self.events
            .retain(|_, (at, _)| now.saturating_duration_since(*at) < TTL);
        self.targets
            .retain(|_, (at, _)| now.saturating_duration_since(*at) < TARGET_TTL);
    }

    pub fn register(&mut self, event: &NostrEvent, label: &str, target: &str, now: Instant) {
        self.prune(now);
        if self.events.len() >= CAPACITY {
            if let Some(key) = self
                .events
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            {
                self.events.remove(&key);
            }
        }
        self.events.insert(
            event.id.clone(),
            (
                now,
                PublishMetadata {
                    kind: event.kind,
                    label: label.to_owned(),
                    target: target.chars().take(8).collect(),
                    relays: Vec::new(),
                },
            ),
        );
    }

    /// Called after a successful websocket write, once for each relay copy.
    pub fn sent(&mut self, frame: &str, relay: &str, now: Instant) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(frame) else {
            return;
        };
        match value[0].as_str() {
            Some("REQ") => {
                self.req += 1;
                let subscription = value[1].as_str().unwrap_or("unknown");
                tracing::debug!("[NostrSend] type=req subscription={subscription} relay={relay}");
            }
            Some("EVENT") => {
                let id = value[1]["id"].as_str().unwrap_or_default();
                if let Some((_, meta)) = self.events.get_mut(id) {
                    if meta.relays.len() < 16 && !meta.relays.iter().any(|url| url == relay) {
                        meta.relays.push(relay.to_owned());
                    }
                    self.counts.entry(meta.label.clone()).or_default().publish += 1;
                    tracing::debug!(
                        "[NostrSend] type={} event_id={} kind={} target={} relay={}",
                        meta.label,
                        id,
                        meta.kind,
                        meta.target,
                        relay
                    );
                }
            }
            _ => {}
        }
        self.prune(now);
    }

    pub fn status(&mut self, id: &str, accepted: bool, message: &str, relay: &str, now: Instant) {
        self.prune(now);
        let meta = self.events.get(id).map(|(_, meta)| meta.clone());
        let label = meta.as_ref().map_or("unknown", |m| m.label.as_str());
        let counts = self.counts.entry(label.to_owned()).or_default();
        if accepted {
            counts.accepted += 1;
        } else {
            counts.rejected += 1;
        }
        let kind = meta.as_ref().map_or(0, |m| m.kind);
        let target = meta.as_ref().map_or("unknown", |m| m.target.as_str());
        if accepted {
            tracing::debug!("[NostrOK] event_id={id} type={label} kind={kind} target={target} relay={relay} accepted=true message={message:?}");
        } else {
            tracing::warn!("[NostrOK] event_id={id} type={label} kind={kind} target={target} relay={relay} accepted=false message={message:?}");
        }
    }

    pub fn discovery(&mut self, pubkey: &str, now: Instant) {
        self.prune(now);
        if !self.targets.contains_key(pubkey) && self.targets.len() >= CAPACITY {
            if let Some(key) = self
                .targets
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            {
                self.targets.remove(&key);
            }
        }
        self.targets
            .entry(pubkey.to_owned())
            .or_insert((now, false))
            .0 = now;
    }

    pub fn response(&mut self, pubkey: &str, now: Instant) {
        self.discovery(pubkey, now);
        if let Some(target) = self.targets.get_mut(pubkey) {
            target.1 = true;
        }
    }

    pub fn received(&mut self) {
        self.received += 1;
    }
    pub fn reset(&mut self) {
        self.resets += 1;
    }

    /// Cumulative counts survive session resets. A report can therefore be
    /// differenced over any interval, including across identity rotation.
    pub fn report(&mut self, node: &str, now: Instant, force: bool) {
        if !force
            && self
                .last_report
                .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(10))
        {
            return;
        }
        self.prune(now);
        self.last_report = Some(now);
        let unanswered = self
            .targets
            .values()
            .filter(|(_, responded)| !responded)
            .count();
        tracing::info!("[NostrStats] node={node} cumulative=true req={} resets={} received={} targets={} unanswered={unanswered}", self.req, self.resets, self.received, self.targets.len());
        for (label, counts) in &self.counts {
            tracing::info!("[NostrStats] node={node} cumulative=true type={label} publish={} accepted={} rejected={}", counts.publish, counts.accepted, counts.rejected);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(id: String) -> NostrEvent {
        NostrEvent {
            id,
            pubkey: String::new(),
            created_at: 0,
            kind: 25050,
            tags: vec![],
            content: String::new(),
            sig: String::new(),
        }
    }
    #[test]
    fn relay_copies_are_counted_separately_and_event_bytes_are_unchanged() {
        let event = event("multi".into());
        let before = serde_json::to_string(&event).unwrap();
        let frame = super::super::event_frame_json(&event).unwrap();
        let now = Instant::now();
        let mut stats = NostrMeasurements::default();
        stats.register(&event, "candidate_ack", "abcdef012345", now);
        for relay in ["ws://127.0.0.1/one", "ws://127.0.0.1/two"] {
            stats.sent(&frame, relay, now);
            stats.status(&event.id, true, "", relay, now);
        }
        assert_eq!(stats.counts["candidate_ack"].publish, 2);
        assert_eq!(stats.counts["candidate_ack"].accepted, 2);
        assert_eq!(stats.events["multi"].1.relays.len(), 2);
        stats.status("expired", false, "rate-limited:", "ws://127.0.0.1", now);
        assert_eq!(stats.counts["unknown"].rejected, 1);
        assert_eq!(serde_json::to_string(&event).unwrap(), before);
    }

    #[test]
    fn retention_is_bounded_and_expires_without_affecting_counts() {
        let mut stats = NostrMeasurements::default();
        let now = Instant::now();
        for i in 0..=CAPACITY {
            stats.register(
                &event(i.to_string()),
                "offer",
                "abcdef012345",
                now + Duration::from_millis(i as u64),
            );
        }
        assert_eq!(stats.events.len(), CAPACITY);
        assert!(!stats.events.contains_key("0"));
        assert_eq!(stats.events["1"].1.target, "abcdef01");
        stats.sent("[\"EVENT\",{\"id\":\"1\"}]", "ws://127.0.0.1", now);
        stats.status("1", false, "rate-limited:", "ws://127.0.0.1", now);
        stats.reset();
        stats.prune(now + TTL + Duration::from_secs(3));
        assert!(stats.events.is_empty());
        assert_eq!(stats.counts["offer"].publish, 1);
        assert_eq!(stats.counts["offer"].rejected, 1);
        assert_eq!(stats.resets, 1);
    }
    #[test]
    fn unanswered_survives_reset_and_recognizes_late_responses() {
        let mut stats = NostrMeasurements::default();
        let now = Instant::now();
        stats.discovery("old", now);
        stats.reset();
        stats.discovery("new", now);
        stats.response("new", now);
        assert_eq!(
            stats
                .targets
                .values()
                .filter(|(_, response)| !response)
                .count(),
            1
        );
        stats.prune(now + TARGET_TTL);
        assert!(stats.targets.is_empty());
    }
}
