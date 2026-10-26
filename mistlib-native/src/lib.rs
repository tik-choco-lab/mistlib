pub mod app;
pub mod build_info;
pub mod config;
pub mod engine;
pub mod error;
pub mod events;
pub mod ffi;
pub mod layers;
pub mod logging;
pub mod media;
pub mod mem;
pub mod runtime;
pub mod signaling;
pub mod storage;
pub mod transports;

pub use app::{
    clear_raw_handler, get_config, get_connected_nodes, get_connected_nodes_async,
    get_connection_state, get_connection_state_async, get_connection_state_value,
    get_connection_state_value_async, get_room_connections, get_room_connections_async, get_stats,
    init, init_and_join, init_with_config, join_room, leave_all_rooms, leave_room, leave_room_id,
    publish_local_track, register_event_callback, register_event_callback_v2, register_local_track,
    register_log_callback, register_media_track_handler, register_media_track_handler_async,
    register_raw_handler, remove_local_track, send_message, send_message_direct,
    send_message_direct_wait, send_message_direct_wait_in_room, send_message_in_room, set_config,
    set_local_track_audience, shutdown, try_send_message, try_send_message_in_room,
    unpublish_local_track, update_position, update_position_in_room, DELIVERY_RELIABLE,
    DELIVERY_RELIABLE_UNORDERED, DELIVERY_UNRELIABLE, DELIVERY_UNRELIABLE_ORDERED,
};
pub use error::{MistError, Result};
pub use events::*;
pub use layers::native_l1;
pub use transports::webrtc::MediaTrackEvent;

/// Re-exported so downstream crates can name the WebRTC types carried by
/// [`MediaTrackEvent`] (TrackRemote, RTCP packets, rtp::packet::Packet, ...)
/// without pinning their own copy of the `webrtc` crate to a matching version.
pub use webrtc;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
