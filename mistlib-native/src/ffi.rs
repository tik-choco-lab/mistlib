use crate::engine::{EventCallback, LogCallback};
use mistlib_core::types::NodeId;

pub const DELIVERY_RELIABLE: u32 = crate::app::DELIVERY_RELIABLE;
pub const DELIVERY_UNRELIABLE_ORDERED: u32 = crate::app::DELIVERY_UNRELIABLE_ORDERED;
pub const DELIVERY_UNRELIABLE: u32 = crate::app::DELIVERY_UNRELIABLE;

#[no_mangle]
pub extern "C" fn join_room(room_ptr: *const u8, room_len: usize) {
    let room_raw = unsafe { std::slice::from_raw_parts(room_ptr, room_len) };
    let room_id = String::from_utf8_lossy(room_raw).to_string();
    crate::app::join_room(room_id);
}

#[no_mangle]
pub extern "C" fn register_log_callback(cb: LogCallback) {
    crate::app::register_log_callback(cb);
}

#[no_mangle]
pub extern "C" fn register_event_callback(cb: EventCallback) {
    crate::app::register_event_callback(cb);
}

#[no_mangle]
pub extern "C" fn init(id_ptr: *const u8, id_len: usize, url_ptr: *const u8, url_len: usize) {
    let id_raw = unsafe { std::slice::from_raw_parts(id_ptr, id_len) };
    let local_id = String::from_utf8_lossy(id_raw).to_string();

    let url_raw = unsafe { std::slice::from_raw_parts(url_ptr, url_len) };
    let signaling_url = String::from_utf8_lossy(url_raw).to_string();

    crate::app::init(local_id, signaling_url);
}

#[no_mangle]
pub extern "C" fn leave_room() {
    crate::app::leave_room();
}

#[no_mangle]
pub extern "C" fn update_position(x: f32, y: f32, z: f32) {
    crate::app::update_position(x, y, z);
}

#[no_mangle]
pub extern "C" fn on_connected(node_ptr: *const u8, node_len: usize) {
    let node_raw = unsafe { std::slice::from_raw_parts(node_ptr, node_len) };
    let node_id = NodeId(String::from_utf8_lossy(node_raw).to_string());
    crate::app::on_connected(node_id);
}

#[no_mangle]
pub extern "C" fn on_disconnected(node_ptr: *const u8, node_len: usize) {
    let node_raw = unsafe { std::slice::from_raw_parts(node_ptr, node_len) };
    let node_id = NodeId(String::from_utf8_lossy(node_raw).to_string());
    crate::app::on_disconnected(node_id);
}

#[no_mangle]
pub extern "C" fn set_config(data: *const u8, len: usize) {
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    crate::app::set_config(slice);
}

#[no_mangle]
pub extern "C" fn send_message(
    target_ptr: *const u8,
    target_len: usize,
    data_ptr: *const u8,
    data_len: usize,
    method: u32,
) {
    let target_raw = unsafe { std::slice::from_raw_parts(target_ptr, target_len) };
    let target_id = String::from_utf8_lossy(target_raw).to_string();

    let data_raw = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };

    crate::app::send_message(target_id, data_raw, method);
}

#[no_mangle]
pub extern "C" fn get_stats(buffer: *mut u8, buffer_len: usize) -> u32 {
    let json_str = crate::app::get_stats();

    let bytes = json_str.as_bytes();
    if bytes.len() > buffer_len {
        return 0;
    }

    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
    }
    bytes.len() as u32
}

#[no_mangle]
pub extern "C" fn get_config(buffer: *mut u8, buffer_len: usize) -> u32 {
    let json_str = crate::app::get_config();

    let bytes = json_str.as_bytes();
    if bytes.len() > buffer_len {
        return 0;
    }

    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
    }
    bytes.len() as u32
}
