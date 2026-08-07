use wasm_bindgen::prelude::*;
use web_sys::MediaStreamTrack;

pub const EVENT_RAW: u32 = crate::app::EVENT_RAW;
pub const EVENT_OVERLAY: u32 = crate::app::EVENT_OVERLAY;
pub const EVENT_NEIGHBORS: u32 = crate::app::EVENT_NEIGHBORS;
pub const EVENT_AOI_ENTERED: u32 = crate::app::EVENT_AOI_ENTERED;
pub const EVENT_AOI_LEFT: u32 = crate::app::EVENT_AOI_LEFT;
pub const EVENT_PEER_CONNECTED: u32 = crate::app::EVENT_PEER_CONNECTED;
pub const EVENT_PEER_DISCONNECTED: u32 = crate::app::EVENT_PEER_DISCONNECTED;
pub const EVENT_AOI_NODES: u32 = crate::app::EVENT_AOI_NODES;
pub const EVENT_ROOM_JOINED: u32 = crate::app::EVENT_ROOM_JOINED;
pub const EVENT_ROOM_JOIN_FAILED: u32 = crate::app::EVENT_ROOM_JOIN_FAILED;
pub const EVENT_ROOM_LEFT: u32 = crate::app::EVENT_ROOM_LEFT;
pub const MEDIA_EVENT_TRACK_ADDED: u32 = crate::app::MEDIA_EVENT_TRACK_ADDED;
pub const MEDIA_EVENT_TRACK_REMOVED: u32 = crate::app::MEDIA_EVENT_TRACK_REMOVED;

pub const DELIVERY_RELIABLE: u32 = crate::app::DELIVERY_RELIABLE;
pub const DELIVERY_UNRELIABLE_ORDERED: u32 = crate::app::DELIVERY_UNRELIABLE_ORDERED;
pub const DELIVERY_UNRELIABLE: u32 = crate::app::DELIVERY_UNRELIABLE;

// wasm-bindgen cannot export a free `pub const`, so the constants above are
// invisible to JS -- importing `EVENT_PEER_CONNECTED` from the generated module
// is a link-time error that takes the whole import statement down with it. These
// enums are what JS actually sees; wasm-bindgen emits them as frozen objects
// (`MistEvent.PeerConnected === 5`) plus matching TypeScript declarations.
// The consts stay for Rust callers, and the const assertions below make the two
// impossible to drift apart.

/// Event ids passed as the first argument of the `register_event_callback`
/// callback. Media events are numbered separately, see [`MistMediaEvent`].
#[wasm_bindgen]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MistEvent {
    Raw = 0,
    Overlay = 1,
    Neighbors = 2,
    AoiEntered = 3,
    AoiLeft = 4,
    PeerConnected = 5,
    PeerDisconnected = 6,
    AoiNodes = 7,
    RoomJoined = 8,
    RoomJoinFailed = 9,
    RoomLeft = 10,
}

/// Event ids passed to the `register_media_event_callback` callback. Numbered
/// from 100 so they never collide with [`MistEvent`].
#[wasm_bindgen]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MistMediaEvent {
    TrackAdded = 100,
    TrackRemoved = 101,
}

/// Delivery guarantee for `send_message`, mapping onto the underlying WebRTC
/// data channel configuration.
#[wasm_bindgen]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Arrives, and in order.
    Reliable = 0,
    /// May be dropped, but never arrives out of order.
    UnreliableOrdered = 1,
    /// May be dropped or reordered. Lowest latency.
    Unreliable = 2,
}

// A mismatch here means JS and Rust disagree about a wire-visible number, which
// would surface as silently misrouted events rather than as a build failure.
const _: () = {
    assert!(MistEvent::Raw as u32 == EVENT_RAW);
    assert!(MistEvent::Overlay as u32 == EVENT_OVERLAY);
    assert!(MistEvent::Neighbors as u32 == EVENT_NEIGHBORS);
    assert!(MistEvent::AoiEntered as u32 == EVENT_AOI_ENTERED);
    assert!(MistEvent::AoiLeft as u32 == EVENT_AOI_LEFT);
    assert!(MistEvent::PeerConnected as u32 == EVENT_PEER_CONNECTED);
    assert!(MistEvent::PeerDisconnected as u32 == EVENT_PEER_DISCONNECTED);
    assert!(MistEvent::AoiNodes as u32 == EVENT_AOI_NODES);
    assert!(MistEvent::RoomJoined as u32 == EVENT_ROOM_JOINED);
    assert!(MistEvent::RoomJoinFailed as u32 == EVENT_ROOM_JOIN_FAILED);
    assert!(MistEvent::RoomLeft as u32 == EVENT_ROOM_LEFT);
    assert!(MistMediaEvent::TrackAdded as u32 == MEDIA_EVENT_TRACK_ADDED);
    assert!(MistMediaEvent::TrackRemoved as u32 == MEDIA_EVENT_TRACK_REMOVED);
    assert!(Delivery::Reliable as u32 == DELIVERY_RELIABLE);
    assert!(Delivery::UnreliableOrdered as u32 == DELIVERY_UNRELIABLE_ORDERED);
    assert!(Delivery::Unreliable as u32 == DELIVERY_UNRELIABLE);
};

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
pub fn init_with_config(id: String, config: String) -> bool {
    crate::app::init_with_config(id, config)
}

#[wasm_bindgen]
pub fn update_position(x: f32, y: f32, z: f32) {
    crate::app::update_position(x, y, z);
}

#[wasm_bindgen]
pub fn update_position_in_room(room_id: String, x: f32, y: f32, z: f32) -> Result<(), JsValue> {
    crate::app::update_position_in_room(room_id, x, y, z)
}

#[wasm_bindgen]
pub fn get_neighbors() -> String {
    crate::app::get_neighbors()
}

#[wasm_bindgen]
pub fn get_neighbors_in_room(room_id: String) -> Result<String, JsValue> {
    crate::app::get_neighbors_in_room(room_id)
}

#[wasm_bindgen]
pub fn get_all_nodes() -> String {
    crate::app::get_all_nodes()
}

#[wasm_bindgen]
pub fn get_all_nodes_in_room(room_id: String) -> Result<String, JsValue> {
    crate::app::get_all_nodes_in_room(room_id)
}

#[wasm_bindgen]
pub fn join_room(room_id: String) {
    crate::app::join_room(room_id);
}

#[wasm_bindgen]
pub fn join_room_async(room_id: String) -> js_sys::Promise {
    crate::app::join_room_async(room_id)
}

#[wasm_bindgen]
pub fn is_room_joined(room_id: String) -> bool {
    crate::app::is_room_joined(room_id)
}

#[wasm_bindgen]
pub fn leave_room() {
    crate::app::leave_room();
}

#[wasm_bindgen]
pub fn leave_room_id(room_id: String) -> Result<(), JsValue> {
    crate::app::leave_room_id(room_id)
}

#[wasm_bindgen]
pub fn leave_room_id_async(room_id: String) -> js_sys::Promise {
    crate::app::leave_room_id_async(room_id)
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
pub fn send_message_in_room(
    room_id: String,
    target_id: String,
    data: &[u8],
    method: u32,
) -> Result<(), JsValue> {
    crate::app::send_message_in_room(room_id, target_id, data, method)
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
