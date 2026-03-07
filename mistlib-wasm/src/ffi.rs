use wasm_bindgen::prelude::*;
use web_sys::MediaStreamTrack;

pub const EVENT_RAW: u32 = crate::app::EVENT_RAW;
pub const EVENT_OVERLAY: u32 = crate::app::EVENT_OVERLAY;
pub const EVENT_NEIGHBORS: u32 = crate::app::EVENT_NEIGHBORS;
pub const MEDIA_EVENT_TRACK_ADDED: u32 = crate::app::MEDIA_EVENT_TRACK_ADDED;
pub const MEDIA_EVENT_TRACK_REMOVED: u32 = crate::app::MEDIA_EVENT_TRACK_REMOVED;

pub const DELIVERY_RELIABLE: u32 = crate::app::DELIVERY_RELIABLE;
pub const DELIVERY_UNRELIABLE_ORDERED: u32 = crate::app::DELIVERY_UNRELIABLE_ORDERED;
pub const DELIVERY_UNRELIABLE: u32 = crate::app::DELIVERY_UNRELIABLE;

#[wasm_bindgen]
pub fn register_event_callback(callback: &js_sys::Function) {
    crate::app::register_event_callback(callback);
}

#[wasm_bindgen]
pub fn register_media_event_callback(callback: &js_sys::Function) {
    crate::app::register_media_event_callback(callback);
}

#[wasm_bindgen]
pub fn init(id: String, url: String) {
    crate::app::init(id, url);
}

#[wasm_bindgen]
pub fn update_position(x: f32, y: f32, z: f32) {
    crate::app::update_position(x, y, z);
}

#[wasm_bindgen]
pub fn get_neighbors() -> String {
    crate::app::get_neighbors()
}

#[wasm_bindgen]
pub fn get_all_nodes() -> String {
    crate::app::get_all_nodes()
}

#[wasm_bindgen]
pub fn join_room(room_id: String) {
    crate::app::join_room(room_id);
}

#[wasm_bindgen]
pub fn leave_room() {
    crate::app::leave_room();
}

#[wasm_bindgen]
pub fn set_config(data: String) -> bool {
    crate::app::set_config(data)
}

#[wasm_bindgen]
pub fn send_message(target_id: String, data: &[u8], method: u32) {
    crate::app::send_message(target_id, data, method);
}

#[wasm_bindgen]
pub fn get_config() -> String {
    crate::app::get_config()
}

#[wasm_bindgen]
pub fn get_stats() -> String {
    crate::app::get_stats()
}

#[wasm_bindgen]
pub fn register_local_track(track_id: String, track: MediaStreamTrack) -> Result<(), JsValue> {
    crate::app::register_local_track(track_id, track)
}

#[wasm_bindgen]
pub fn get_local_track(track_id: String) -> Result<Option<MediaStreamTrack>, JsValue> {
    crate::app::get_local_track(track_id)
}

#[wasm_bindgen]
pub fn publish_local_track(track_id: String) -> Result<(), JsValue> {
    crate::app::publish_local_track(track_id)
}

#[wasm_bindgen]
pub fn unpublish_local_track(track_id: String) -> Result<(), JsValue> {
    crate::app::unpublish_local_track(track_id)
}

#[wasm_bindgen]
pub fn remove_local_track(track_id: String) -> Result<(), JsValue> {
    crate::app::remove_local_track(track_id)
}

#[wasm_bindgen]
pub fn set_local_track_enabled(track_id: String, enabled: bool) -> Result<(), JsValue> {
    crate::app::set_local_track_enabled(track_id, enabled)
}
