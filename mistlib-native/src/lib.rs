pub mod app;
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
    init, init_and_join, init_with_config, join_room, leave_room, leave_room_id,
    publish_local_track, register_event_callback, register_event_callback_v2,
    register_log_callback, register_media_track_handler, register_media_track_handler_async,
    register_raw_handler, send_message, send_message_direct, send_message_in_room, set_config,
    shutdown, try_send_message, try_send_message_in_room, unpublish_local_track, update_position,
    update_position_in_room, DELIVERY_RELIABLE, DELIVERY_UNRELIABLE, DELIVERY_UNRELIABLE_ORDERED,
};
pub use error::{MistError, Result};
pub use events::{
    EventCallback, EventCallbackV2, RustEventCallback, EVENT_ALL_CONNECTIONS_LOST,
    EVENT_AOI_ENTERED, EVENT_AOI_LEFT, EVENT_AOI_NODES, EVENT_JOIN, EVENT_LEAVE, EVENT_NEIGHBORS,
    EVENT_NODE_POSITION_UPDATED, EVENT_OVERLAY, EVENT_RAW,
};
pub use layers::native_l1;
pub use transports::webrtc::MediaTrackEvent;

/// Re-exported so downstream crates can name the WebRTC types carried by
/// [`MediaTrackEvent`] (TrackRemote, RTCP packets, rtp::packet::Packet, ...)
/// without pinning their own copy of the `webrtc` crate to a matching version.
pub use webrtc;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
