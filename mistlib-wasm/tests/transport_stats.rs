#![cfg(target_arch = "wasm32")]

#[path = "../src/transport/webrtc/transport_activity.rs"]
mod implementation;

use js_sys::{Map, JSON};
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

fn sample(reports: &[&str]) -> Option<(String, u64)> {
    let stats = Map::new();
    for (index, report) in reports.iter().enumerate() {
        stats.set(&JsValue::from(index as u32), &JSON::parse(report).unwrap());
    }
    implementation::received_bytes(stats.as_ref())
}

#[wasm_bindgen_test]
fn chrome_transport_and_firefox_selected_pair_shapes() {
    assert_eq!(
        sample(&[
            r#"{"id":"transport","type":"transport","bytesReceived":1234,"selectedCandidatePairId":"pair"}"#,
            r#"{"id":"pair","type":"candidate-pair","bytesReceived":1000,"state":"succeeded"}"#,
        ]),
        Some(("transport".into(), 1234))
    );
    assert_eq!(
        sample(&[
            r#"{"id":"transport","type":"transport","selectedCandidatePairId":"pair"}"#,
            r#"{"id":"wrong","type":"candidate-pair","bytesReceived":99999,"selected":true}"#,
            r#"{"id":"pair","type":"candidate-pair","bytesReceived":1000,"state":"succeeded"}"#,
        ]),
        Some(("pair".into(), 1000))
    );
    for selection in [
        r#""selected":true"#,
        r#""nominated":true,"state":"succeeded""#,
    ] {
        let pair =
            format!(r#"{{"id":"pair","type":"candidate-pair","bytesReceived":456,{selection}}}"#);
        assert_eq!(sample(&[&pair]), Some(("pair".into(), 456)));
    }
}

#[wasm_bindgen_test]
fn missing_invalid_or_unselected_counters_are_not_activity() {
    for report in [
        r#"{"id":"pair","type":"candidate-pair","bytesReceived":1000,"state":"succeeded"}"#,
        r#"{"id":"pair","type":"candidate-pair","bytesReceived":1000,"nominated":true,"state":"in-progress"}"#,
        r#"{"id":"t","type":"transport"}"#,
        r#"{"id":"t","type":"transport","bytesReceived":-1}"#,
        r#"{"id":"t","type":"transport","bytesReceived":1.5}"#,
        r#"{"id":"dc","type":"data-channel","bytesReceived":1000}"#,
    ] {
        assert_eq!(sample(&[report]), None, "{report}");
    }
    assert_eq!(sample(&[]), None);
    assert_eq!(implementation::received_bytes(&JsValue::NULL), None);
}
