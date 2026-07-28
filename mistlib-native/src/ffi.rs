use crate::engine::{EventCallback, EventCallbackV2, LogCallback};
use mistlib_core::types::NodeId;

pub const DELIVERY_RELIABLE: u32 = crate::app::DELIVERY_RELIABLE;
pub const DELIVERY_UNRELIABLE_ORDERED: u32 = crate::app::DELIVERY_UNRELIABLE_ORDERED;
pub const DELIVERY_UNRELIABLE: u32 = crate::app::DELIVERY_UNRELIABLE;

#[no_mangle]
/// # Safety
/// `room_ptr` must be valid for `room_len` bytes.
pub unsafe extern "C" fn join_room(room_ptr: *const u8, room_len: usize) {
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
/// v2: same events as `register_event_callback`, tagged with the room_id
/// they occurred in (SPEC-15). If both v1 and v2 are registered, both fire.
pub extern "C" fn register_event_callback_v2(cb: EventCallbackV2) {
    crate::app::register_event_callback_v2(cb);
}

#[no_mangle]
/// # Safety
/// `id_ptr` must be valid for `id_len` bytes and `url_ptr` must be valid for `url_len` bytes.
pub unsafe extern "C" fn init(
    id_ptr: *const u8,
    id_len: usize,
    url_ptr: *const u8,
    url_len: usize,
) {
    let id_raw = unsafe { std::slice::from_raw_parts(id_ptr, id_len) };
    let local_id = String::from_utf8_lossy(id_raw).to_string();

    let url_raw = unsafe { std::slice::from_raw_parts(url_ptr, url_len) };
    let signaling_url = String::from_utf8_lossy(url_raw).to_string();

    crate::app::init(local_id, signaling_url);
}

#[no_mangle]
/// # Safety
/// `id_ptr` must be valid for `id_len` bytes and `config_ptr` must be valid for `config_len` bytes.
pub unsafe extern "C" fn init_with_config(
    id_ptr: *const u8,
    id_len: usize,
    config_ptr: *const u8,
    config_len: usize,
) -> bool {
    let id_raw = unsafe { std::slice::from_raw_parts(id_ptr, id_len) };
    let local_id = String::from_utf8_lossy(id_raw).to_string();

    let config_raw = unsafe { std::slice::from_raw_parts(config_ptr, config_len) };

    crate::app::init_with_config(local_id, config_raw)
}

#[no_mangle]
pub extern "C" fn leave_room() {
    crate::app::leave_room();
}

#[no_mangle]
/// Leaves only `room_id`'s session, leaving every other active room
/// untouched. Not-joined is a no-op.
///
/// # Safety
/// `room_ptr` must be valid for `room_len` bytes.
pub unsafe extern "C" fn leave_room_id(room_ptr: *const u8, room_len: usize) {
    let room_raw = unsafe { std::slice::from_raw_parts(room_ptr, room_len) };
    let room_id = String::from_utf8_lossy(room_raw).to_string();
    crate::app::leave_room_id(room_id);
}

#[no_mangle]
pub extern "C" fn update_position(x: f32, y: f32, z: f32) {
    crate::app::update_position(x, y, z);
}

#[no_mangle]
/// Room-scoped `update_position`: only `room_id`'s session sees it.
///
/// # Safety
/// `room_ptr` must be valid for `room_len` bytes.
pub unsafe extern "C" fn update_position_in_room(
    room_ptr: *const u8,
    room_len: usize,
    x: f32,
    y: f32,
    z: f32,
) {
    let room_raw = unsafe { std::slice::from_raw_parts(room_ptr, room_len) };
    let room_id = String::from_utf8_lossy(room_raw).to_string();
    crate::app::update_position_in_room(room_id, x, y, z);
}

#[no_mangle]
/// # Safety
/// `node_ptr` must be valid for `node_len` bytes.
pub unsafe extern "C" fn on_connected(node_ptr: *const u8, node_len: usize) {
    let node_raw = unsafe { std::slice::from_raw_parts(node_ptr, node_len) };
    let node_id = NodeId(String::from_utf8_lossy(node_raw).to_string());
    crate::app::on_connected(node_id);
}

#[no_mangle]
/// # Safety
/// `node_ptr` must be valid for `node_len` bytes.
pub unsafe extern "C" fn on_disconnected(node_ptr: *const u8, node_len: usize) {
    let node_raw = unsafe { std::slice::from_raw_parts(node_ptr, node_len) };
    let node_id = NodeId(String::from_utf8_lossy(node_raw).to_string());
    crate::app::on_disconnected(node_id);
}

#[no_mangle]
/// # Safety
/// `data` must be valid for `len` bytes.
pub unsafe extern "C" fn set_config(data: *const u8, len: usize) {
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    crate::app::set_config(slice);
}

#[no_mangle]
/// # Safety
/// `target_ptr` must be valid for `target_len` bytes and `data_ptr` must be valid for `data_len` bytes.
pub unsafe extern "C" fn send_message(
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
/// Room-scoped `send_message`: errors (logged, not delivered) if `room_id`
/// isn't currently joined, instead of falling back to another room.
///
/// # Safety
/// `room_ptr`, `target_ptr`, and `data_ptr` must be valid for their
/// respective `*_len` byte counts.
pub unsafe extern "C" fn send_message_in_room(
    room_ptr: *const u8,
    room_len: usize,
    target_ptr: *const u8,
    target_len: usize,
    data_ptr: *const u8,
    data_len: usize,
    method: u32,
) {
    let room_raw = unsafe { std::slice::from_raw_parts(room_ptr, room_len) };
    let room_id = String::from_utf8_lossy(room_raw).to_string();

    let target_raw = unsafe { std::slice::from_raw_parts(target_ptr, target_len) };
    let target_id = String::from_utf8_lossy(target_raw).to_string();

    let data_raw = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };

    crate::app::send_message_in_room(room_id, target_id, data_raw, method);
}

#[no_mangle]
/// # Safety
/// `buffer` must be valid for `buffer_len` bytes.
pub unsafe extern "C" fn get_stats(buffer: *mut u8, buffer_len: usize) -> u32 {
    let json_str = crate::app::get_stats();

    let bytes = json_str.as_bytes();
    if bytes.len() > buffer_len {
        return 0;
    }

    // SAFETY: buffer is valid for buffer_len bytes; we checked bytes.len() <= buffer_len above.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
    }
    bytes.len() as u32
}

#[no_mangle]
/// Stores `data` in the P2P storage and writes the resulting CID (UTF-8) into `cid_buffer`.
///
/// Returns the CID byte length on success, or 0 on failure (storage uninitialized,
/// add error, or `cid_buffer` too small).
///
/// # Safety
/// `name_ptr` must be valid for `name_len` bytes, `data_ptr` for `data_len` bytes,
/// and `cid_buffer` for `cid_buffer_len` bytes.
pub unsafe extern "C" fn storage_add(
    name_ptr: *const u8,
    name_len: usize,
    data_ptr: *const u8,
    data_len: usize,
    cid_buffer: *mut u8,
    cid_buffer_len: usize,
) -> u32 {
    let name_raw = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
    let name = String::from_utf8_lossy(name_raw).to_string();

    let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };

    let cid = match crate::app::storage_add(&name, data) {
        Ok(cid) => cid,
        Err(_) => return 0,
    };

    let bytes = cid.as_bytes();
    if bytes.len() > cid_buffer_len {
        return 0;
    }

    // SAFETY: cid_buffer is valid for cid_buffer_len bytes; we checked the length above.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), cid_buffer, bytes.len());
    }
    bytes.len() as u32
}

#[no_mangle]
/// Explicit-position variant of `storage_add` (SPEC-16): stores `data` in the
/// P2P storage tagged with world position `(x, y, z)` instead of relying on
/// auto-tagging from the caller's last `update_position`, and writes the
/// resulting CID (UTF-8) into `cid_buffer`.
///
/// Returns the CID byte length on success, or 0 on failure (storage
/// uninitialized, add error, or `cid_buffer` too small).
///
/// # Safety
/// `name_ptr` must be valid for `name_len` bytes, `data_ptr` for `data_len` bytes,
/// and `cid_buffer` for `cid_buffer_len` bytes.
pub unsafe extern "C" fn storage_add_at(
    name_ptr: *const u8,
    name_len: usize,
    data_ptr: *const u8,
    data_len: usize,
    x: f32,
    y: f32,
    z: f32,
    cid_buffer: *mut u8,
    cid_buffer_len: usize,
) -> u32 {
    let name_raw = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
    let name = String::from_utf8_lossy(name_raw).to_string();

    let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };

    let position = Some(mistlib_core::types::Vector3::new(x, y, z));
    let cid = match crate::app::storage_add_at(&name, data, position) {
        Ok(cid) => cid,
        Err(_) => return 0,
    };

    let bytes = cid.as_bytes();
    if bytes.len() > cid_buffer_len {
        return 0;
    }

    // SAFETY: cid_buffer is valid for cid_buffer_len bytes; we checked the length above.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), cid_buffer, bytes.len());
    }
    bytes.len() as u32
}

#[no_mangle]
/// Retrieves the data for `cid` and writes it into `buffer`.
///
/// Returns the number of bytes written on success. If `buffer` is too small the
/// data is not copied and the required length is returned, so callers can query
/// the size by passing a zero-length buffer first. Returns 0 on failure
/// (storage uninitialized or get error).
///
/// # Safety
/// `cid_ptr` must be valid for `cid_len` bytes and `buffer` for `buffer_len` bytes.
pub unsafe extern "C" fn storage_get(
    cid_ptr: *const u8,
    cid_len: usize,
    buffer: *mut u8,
    buffer_len: usize,
) -> u32 {
    let cid_raw = unsafe { std::slice::from_raw_parts(cid_ptr, cid_len) };
    let cid = String::from_utf8_lossy(cid_raw).to_string();

    let data = match crate::app::storage_get(&cid) {
        Ok(data) => data,
        Err(_) => return 0,
    };

    if data.len() > buffer_len {
        // Buffer too small: report the required length without copying.
        return data.len() as u32;
    }

    // SAFETY: buffer is valid for buffer_len bytes; we checked data.len() <= buffer_len above.
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), buffer, data.len());
    }
    data.len() as u32
}

#[no_mangle]
/// # Safety
/// `buffer` must be valid for `buffer_len` bytes.
pub unsafe extern "C" fn get_config(buffer: *mut u8, buffer_len: usize) -> u32 {
    let json_str = crate::app::get_config();

    let bytes = json_str.as_bytes();
    if bytes.len() > buffer_len {
        return 0;
    }

    // SAFETY: buffer is valid for buffer_len bytes; we checked bytes.len() <= buffer_len above.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
    }
    bytes.len() as u32
}
