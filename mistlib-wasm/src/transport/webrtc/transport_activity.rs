//! Browser stats are map-like RTCStatsReports. Reflect avoids extra web-sys
//! features and tolerates Chrome's transport reports and Firefox's pair reports.
use js_sys::{Array, Reflect};
use wasm_bindgen::JsValue;

fn field(value: &JsValue, key: &str) -> Option<JsValue> {
    Reflect::get(value, &JsValue::from_str(key)).ok()
}

fn bytes(report: &JsValue) -> Option<(String, u64)> {
    let id = field(report, "id")?.as_string()?;
    let bytes = field(report, "bytesReceived")?.as_f64()?;
    // JS counters are doubles. Reject missing/invalid values instead of
    // manufacturing a zero baseline; realistic counters fit exactly in u64.
    (bytes.is_finite() && bytes >= 0.0 && bytes.fract() == 0.0 && bytes < u64::MAX as f64)
        .then_some((id, bytes as u64))
}

pub(super) fn received_bytes(stats: &JsValue) -> Option<(String, u64)> {
    // RTCStatsReport iterates [id, report]. Limit work even for unusual reports.
    let reports: Vec<_> = js_sys::try_iter(stats)
        .ok()??
        .take(4096)
        .filter_map(|entry| entry.ok().map(|entry| Array::from(&entry).get(1)))
        .collect();
    let mut selected_pair = None;
    for report in &reports {
        if field(report, "type").and_then(|v| v.as_string()).as_deref() == Some("transport") {
            if let Some(sample) = bytes(report) {
                return Some(sample);
            }
            selected_pair = field(report, "selectedCandidatePairId").and_then(|v| v.as_string());
        }
    }
    let pair = reports.iter().find(|report| {
        if field(report, "type").and_then(|v| v.as_string()).as_deref() != Some("candidate-pair") {
            return false;
        }
        if let Some(id) = &selected_pair {
            return field(report, "id").and_then(|v| v.as_string()).as_ref() == Some(id);
        }
        field(report, "selected").and_then(|v| v.as_bool()) == Some(true)
            || (field(report, "nominated").and_then(|v| v.as_bool()) == Some(true)
                && field(report, "state")
                    .and_then(|v| v.as_string())
                    .as_deref()
                    == Some("succeeded"))
    })?;
    bytes(pair)
}
