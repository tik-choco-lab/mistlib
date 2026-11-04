//! Per-peer observations, separate from all connection control state.
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct IceMeasurements {
    pub id: u64,
    pub generated: AtomicU64,
    pub received: AtomicU64,
    pub buffered: AtomicU64,
    pub applied: AtomicU64,
    pub apply_failed: AtomicU64,
}
impl Default for IceMeasurements {
    fn default() -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            generated: AtomicU64::new(0),
            received: AtomicU64::new(0),
            buffered: AtomicU64::new(0),
            applied: AtomicU64::new(0),
            apply_failed: AtomicU64::new(0),
        }
    }
}

pub(crate) fn candidate_type(candidate: &str) -> &str {
    let mut words = candidate.split_whitespace();
    while let Some(word) = words.next() {
        if word == "typ" {
            return match words.next() {
                Some("host") => "host",
                Some("srflx") => "srflx",
                Some("relay") => "relay",
                Some("prflx") => "prflx",
                _ => "unknown",
            };
        }
    }
    "unknown"
}

pub(crate) fn json_candidate_type(json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|value| {
            value["candidate"]
                .as_str()
                .map(|s| candidate_type(s).to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classify_without_logging_addresses() {
        for kind in ["host", "srflx", "relay", "prflx"] {
            let candidate = format!("candidate:1 1 udp 1 127.0.0.1 1 typ {kind}");
            assert_eq!(candidate_type(&candidate), kind);
            assert_eq!(
                json_candidate_type(&serde_json::json!({"candidate":candidate}).to_string()),
                kind
            );
        }
        assert_eq!(json_candidate_type("invalid"), "unknown");
        assert_eq!(candidate_type("typ exotic"), "unknown");
    }
}
