#![cfg(target_arch = "wasm32")]
// Node harness: verifies callback bytes/room scoping without browser transports.
use mistlib_core::events::*;
use mistlib_core::types::NodeId;
use mistlib_wasm::{app, ffi};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_test::wasm_bindgen_test;

#[wasm_bindgen(
    inline_js = "export async function flushEvents() { await Promise.resolve(); await Promise.resolve(); }"
)]
extern "C" {
    async fn flushEvents();
}

#[wasm_bindgen_test]
fn wasm_enums_use_the_entire_core_table() {
    macro_rules! check_app {
        ($($name:ident, $constant:ident, $variant:ident, $code:literal;)*) => {
            $(assert_eq!(ffi::MistEvent::$variant as u32, $constant);)*
        };
    }
    macro_rules! check_media {
        ($($name:ident, $constant:ident, $variant:ident, $code:literal;)*) => {
            $(assert_eq!(ffi::MistMediaEvent::$variant as u32, $constant);)*
        };
    }
    mistlib_core::application_event_codes!(check_app);
    mistlib_core::media_event_codes!(check_media);
}

#[wasm_bindgen_test(async)]
async fn callbacks_preserve_peer_payloads_and_room_lifecycle_order() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let captured = seen.clone();
    let callback = Closure::<dyn FnMut(u32, String, js_sys::Uint8Array, String)>::new(
        move |code, from, data: js_sys::Uint8Array, room| {
            captured
                .borrow_mut()
                .push((code, from, data.to_vec(), room));
        },
    );
    app::register_event_callback(callback.as_ref().unchecked_ref());
    app::emit_room_joined("a".into());
    app::emit_peer_connected(NodeId("peer".into()), "a".into());
    app::emit_peer_disconnected(NodeId("peer".into()), "b".into());
    app::emit_room_left("a".into());
    app::emit_room_join_failed("b".into(), "join cancelled by leave".into());
    flushEvents().await;
    assert_eq!(
        *seen.borrow(),
        vec![
            (EVENT_ROOM_JOINED, "".into(), vec![], "a".into()),
            (
                EVENT_PEER_CONNECTED,
                "peer".into(),
                b"joined".to_vec(),
                "a".into()
            ),
            (
                EVENT_PEER_DISCONNECTED,
                "peer".into(),
                b"left".to_vec(),
                "b".into()
            ),
            (EVENT_ROOM_LEFT, "".into(), vec![], "a".into()),
            (
                EVENT_ROOM_JOIN_FAILED,
                "".into(),
                b"join cancelled by leave".to_vec(),
                "b".into()
            ),
        ]
    );
    app::register_event_callback(&js_sys::Function::new_no_args(""));
}
