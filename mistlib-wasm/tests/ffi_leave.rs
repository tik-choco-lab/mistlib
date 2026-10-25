#![cfg(target_arch = "wasm32")]
// Independent Node harness; no browser, signaling, or transport is started.
use mistlib_wasm::ffi;
use wasm_bindgen::prelude::*;
use wasm_bindgen_test::wasm_bindgen_test;

#[wasm_bindgen(inline_js = "
let previousWarn;
let warnings = [];
export function captureWarnings() {
    warnings = [];
    previousWarn = console.warn;
    console.warn = (...args) => warnings.push(args.join(' '));
}
export function warningCount() {
    return warnings.filter(s => s.includes('leave_room()')).length;
}
export function restoreWarnings() {
    console.warn = previousWarn;
    return warningCount();
}
")]
extern "C" {
    fn captureWarnings();
    fn warningCount() -> u32;
    fn restoreWarnings() -> u32;
}

#[wasm_bindgen_test]
fn room_id_is_scoped_and_legacy_no_argument_warns_once() {
    captureWarnings();
    // A specified missing room reports an error; it must never select leave-all.
    let error = ffi::leave_room(Some("missing".into())).unwrap_err();
    assert!(error.as_string().unwrap().contains("Room not joined"));
    assert!(ffi::leave_room(Some(String::new())).is_err());
    assert!(ffi::leave_room_id("missing".into()).is_err());
    ffi::leave_all_rooms();
    assert_eq!(warningCount(), 0);
    // None is the wasm-bindgen representation of an omitted/undefined argument.
    ffi::leave_room(None).unwrap();
    ffi::leave_room(None).unwrap();
    assert_eq!(restoreWarnings(), 1);
}
