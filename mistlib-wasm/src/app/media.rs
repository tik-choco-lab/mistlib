use super::{MEDIA_EVENT_CALLBACK, MEDIA_EVENT_TRACK_ADDED, MEDIA_EVENT_TRACK_REMOVED};
use mistlib_core::types::NodeId;
use wasm_bindgen::prelude::*;
use web_sys::{MediaStream, MediaStreamTrack};

pub fn register_media_event_callback(callback: &js_sys::Function) {
    MEDIA_EVENT_CALLBACK.with(|cb| {
        *cb.borrow_mut() = Some(callback.clone());
    });
}

pub fn emit_media_track_added(
    from: NodeId,
    track_id: String,
    kind: String,
    track: MediaStreamTrack,
    stream: Option<MediaStream>,
) {
    let callback = MEDIA_EVENT_CALLBACK.with(|cb| cb.borrow().as_ref().cloned());
    if let Some(f) = callback {
        wasm_bindgen_futures::spawn_local(async move {
            let event_type = JsValue::from_f64(MEDIA_EVENT_TRACK_ADDED as f64);
            let from_js = JsValue::from_str(&from.0);
            let track_id_js = JsValue::from_str(&track_id);
            let kind_js = JsValue::from_str(&kind);
            let stream_js = stream.map(JsValue::from).unwrap_or(JsValue::UNDEFINED);
            let _ = f.call6(
                &JsValue::NULL,
                &event_type,
                &from_js,
                &track_id_js,
                &kind_js,
                &JsValue::from(track),
                &stream_js,
            );
        });
    }
}

pub fn emit_media_track_removed(from: NodeId, track_id: String, kind: String) {
    let callback = MEDIA_EVENT_CALLBACK.with(|cb| cb.borrow().as_ref().cloned());
    if let Some(f) = callback {
        wasm_bindgen_futures::spawn_local(async move {
            let event_type = JsValue::from_f64(MEDIA_EVENT_TRACK_REMOVED as f64);
            let from_js = JsValue::from_str(&from.0);
            let track_id_js = JsValue::from_str(&track_id);
            let kind_js = JsValue::from_str(&kind);
            let _ = f.call6(
                &JsValue::NULL,
                &event_type,
                &from_js,
                &track_id_js,
                &kind_js,
                &JsValue::UNDEFINED,
                &JsValue::UNDEFINED,
            );
        });
    }
}

// Local media tracks are roomless: registering/publishing one applies to
// every joined session's WebRTC transport, so a track is visible to peers in
// every room (multi-room contract point 11). This is structurally
// straightforward here because each `WasmWebRtcTransport` method is already
// self-contained per-transport (its own `local_tracks`/`peer_senders` maps),
// so looping over all session transports needed no changes to that type.

pub fn register_local_track(track_id: String, track: MediaStreamTrack) -> Result<(), JsValue> {
    let transports = crate::app::all_session_webrtc_transports();
    if transports.is_empty() {
        return Err(JsValue::from_str("WebRTC transport is not initialized"));
    }
    for webrtc in transports {
        webrtc
            .register_local_track(track_id.clone(), track.clone())
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
    }
    Ok(())
}

pub fn get_local_track(track_id: String) -> Result<Option<MediaStreamTrack>, JsValue> {
    let transports = crate::app::all_session_webrtc_transports();
    let Some(first) = transports.first() else {
        return Err(JsValue::from_str("WebRTC transport is not initialized"));
    };
    Ok(first.get_local_track(&track_id))
}

pub fn set_local_track_enabled(track_id: String, enabled: bool) -> Result<(), JsValue> {
    let transports = crate::app::all_session_webrtc_transports();
    if transports.is_empty() {
        return Err(JsValue::from_str("WebRTC transport is not initialized"));
    }
    for webrtc in transports {
        webrtc
            .set_local_track_enabled(&track_id, enabled)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
    }
    Ok(())
}

pub fn publish_local_track(track_id: String) -> Result<(), JsValue> {
    let transports = crate::app::all_session_webrtc_transports();
    if transports.is_empty() {
        return Err(JsValue::from_str("WebRTC transport is not initialized"));
    }
    for webrtc in transports {
        let track_id = track_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(err) = webrtc.publish_local_track(&track_id).await {
                tracing::error!("Failed to publish local track {}: {}", track_id, err);
            }
        });
    }
    Ok(())
}

pub fn unpublish_local_track(track_id: String) -> Result<(), JsValue> {
    let transports = crate::app::all_session_webrtc_transports();
    if transports.is_empty() {
        return Err(JsValue::from_str("WebRTC transport is not initialized"));
    }
    for webrtc in transports {
        let track_id = track_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(err) = webrtc.unpublish_local_track(&track_id).await {
                tracing::error!("Failed to unpublish local track {}: {}", track_id, err);
            }
        });
    }
    Ok(())
}

pub fn remove_local_track(track_id: String) -> Result<(), JsValue> {
    let transports = crate::app::all_session_webrtc_transports();
    if transports.is_empty() {
        return Err(JsValue::from_str("WebRTC transport is not initialized"));
    }
    for webrtc in transports {
        let track_id = track_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(err) = webrtc.remove_local_track(&track_id).await {
                tracing::error!("Failed to remove local track {}: {}", track_id, err);
            }
        });
    }
    Ok(())
}
