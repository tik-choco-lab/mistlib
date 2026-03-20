use crate::layers::wasm_l0::WasmL0;
use crate::transport::webrtc::WasmWebRtcTransport;
use bytes::Bytes;
use mistlib_core::engine::{
    EngineEvent, EngineEventHandler, EngineState, MistEngine, RunningContext,
};
use mistlib_core::layers::L0Engine;
use mistlib_core::signaling::MessageContent;
use mistlib_core::types::DeliveryMethod;
use mistlib_core::types::NodeId;
use std::cell::RefCell;
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use web_sys::{MediaStream, MediaStreamTrack};

thread_local! {
    pub static ENGINE: Arc<MistEngine> = MistEngine::new(Arc::new(crate::runtime::WasmRuntime));
    pub(crate) static EVENT_CALLBACK: RefCell<Option<js_sys::Function>> = RefCell::new(None);
    pub(crate) static MEDIA_EVENT_CALLBACK: RefCell<Option<js_sys::Function>> = RefCell::new(None);
    pub(crate) static WEBRTC_TRANSPORT: RefCell<Option<Arc<WasmWebRtcTransport>>> = RefCell::new(None);
    pub(crate) static PENDING_START: RefCell<Option<(RunningContext, tokio::sync::mpsc::UnboundedReceiver<MessageContent>)>> = RefCell::new(None);
    pub(crate) static L1_TRANSPORT: RefCell<Option<Arc<crate::layers::wasm_l1::WasmL1Transport>>> = RefCell::new(None);
    pub(crate) static L0: WasmL0 = WasmL0::new();
}

pub const EVENT_RAW: u32 = 0;
pub const EVENT_OVERLAY: u32 = 1;
pub const EVENT_NEIGHBORS: u32 = 2;
pub const EVENT_AOI_ENTERED: u32 = 3;
pub const EVENT_AOI_LEFT: u32 = 4;
pub const MEDIA_EVENT_TRACK_ADDED: u32 = 100;
pub const MEDIA_EVENT_TRACK_REMOVED: u32 = 101;
pub const DELIVERY_RELIABLE: u32 = 0;
pub const DELIVERY_UNRELIABLE_ORDERED: u32 = 1;
pub const DELIVERY_UNRELIABLE: u32 = 2;

pub(crate) struct WasmEngineEventHandler;
impl EngineEventHandler for WasmEngineEventHandler {
    fn on_event(&self, event: EngineEvent) {
        if let EngineEvent::RawMessage(from, data) = &event {
            if !data.is_empty() {
                let first = data[0];
                if first == 0x01 {
                    
                    if let Ok(cid) = std::str::from_utf8(&data[1..]) {
                        crate::storage::handle_want(from.clone(), cid.to_string());
                    }
                } else if first == 0x02 {
                    
                    if data.len() >= 2 {
                        let cid_len = data[1] as usize;
                        if data.len() >= 2 + cid_len {
                            if let Ok(cid) = std::str::from_utf8(&data[2..2 + cid_len]) {
                                let payload = data[2 + cid_len..].to_vec();
                                crate::storage::handle_have(cid.to_string(), payload);
                            }
                        }
                    }
                } else if first == 0x03 {
                    
                    if let Ok(cid) = std::str::from_utf8(&data[1..]) {
                        crate::storage::handle_query(from.clone(), cid.to_string());
                    }
                } else if first == 0x04 {
                    
                    if let Ok(cid) = std::str::from_utf8(&data[1..]) {
                        crate::storage::handle_have_status(from.clone(), cid.to_string());
                    }
                } else if first == 0x05 {
                    if let Some((cid, chunk_index, chunk_total, payload)) =
                        crate::storage::resolver::parse_have_chunk_message(data)
                    {
                        crate::storage::WANT_REGISTRY.with(|r| {
                            r.fulfill_chunk(&cid, chunk_index, chunk_total, payload);
                        });
                    }
                }
            }
        }

        let callback = EVENT_CALLBACK.with(|cb| cb.borrow().as_ref().cloned());
        if let Some(f) = callback {
            let (event_type, from_id, payload_vec) = match event {
                EngineEvent::RawMessage(id, data) => (EVENT_RAW, id.0, data.to_vec()),
                EngineEvent::OverlayMessage(id, data) => (EVENT_OVERLAY, id.0, data),
                EngineEvent::NeighborsUpdated(data) => (EVENT_NEIGHBORS, "rust".to_string(), data),
                EngineEvent::AoiEntered(id) => (EVENT_AOI_ENTERED, id.0, vec![]),
                EngineEvent::AoiLeft(id) => (EVENT_AOI_LEFT, id.0, vec![]),
            };
            wasm_bindgen_futures::spawn_local(async move {
                let ev_type = JsValue::from_f64(event_type as f64);
                let from_js = JsValue::from_str(&from_id);
                let bytes = js_sys::Uint8Array::from(payload_vec.as_slice()).into();
                let _ = f.call3(&JsValue::NULL, &ev_type, &from_js, &bytes);
            });
        }
    }
}

pub fn register_event_callback(callback: &js_sys::Function) {
    EVENT_CALLBACK.with(|cb| {
        *cb.borrow_mut() = Some(callback.clone());
    });
}

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

pub fn register_local_track(track_id: String, track: MediaStreamTrack) -> Result<(), JsValue> {
    WEBRTC_TRANSPORT.with(|transport| {
        let transport = transport.borrow();
        let webrtc = transport
            .as_ref()
            .ok_or_else(|| JsValue::from_str("WebRTC transport is not initialized"))?;
        webrtc
            .register_local_track(track_id, track)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    })
}

pub fn get_local_track(track_id: String) -> Result<Option<MediaStreamTrack>, JsValue> {
    WEBRTC_TRANSPORT.with(|transport| {
        let transport = transport.borrow();
        let webrtc = transport
            .as_ref()
            .ok_or_else(|| JsValue::from_str("WebRTC transport is not initialized"))?;
        Ok(webrtc.get_local_track(&track_id))
    })
}

pub fn set_local_track_enabled(track_id: String, enabled: bool) -> Result<(), JsValue> {
    WEBRTC_TRANSPORT.with(|transport| {
        let transport = transport.borrow();
        let webrtc = transport
            .as_ref()
            .ok_or_else(|| JsValue::from_str("WebRTC transport is not initialized"))?;
        webrtc
            .set_local_track_enabled(&track_id, enabled)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    })
}

pub fn publish_local_track(track_id: String) -> Result<(), JsValue> {
    WEBRTC_TRANSPORT.with(|transport| {
        let transport = transport.borrow();
        let webrtc = transport
            .as_ref()
            .ok_or_else(|| JsValue::from_str("WebRTC transport is not initialized"))?
            .clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(err) = webrtc.publish_local_track(&track_id).await {
                tracing::error!("Failed to publish local track {}: {}", track_id, err);
            }
        });
        Ok(())
    })
}

pub fn unpublish_local_track(track_id: String) -> Result<(), JsValue> {
    WEBRTC_TRANSPORT.with(|transport| {
        let transport = transport.borrow();
        let webrtc = transport
            .as_ref()
            .ok_or_else(|| JsValue::from_str("WebRTC transport is not initialized"))?
            .clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(err) = webrtc.unpublish_local_track(&track_id).await {
                tracing::error!("Failed to unpublish local track {}: {}", track_id, err);
            }
        });
        Ok(())
    })
}

pub fn remove_local_track(track_id: String) -> Result<(), JsValue> {
    WEBRTC_TRANSPORT.with(|transport| {
        let transport = transport.borrow();
        let webrtc = transport
            .as_ref()
            .ok_or_else(|| JsValue::from_str("WebRTC transport is not initialized"))?
            .clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(err) = webrtc.remove_local_track(&track_id).await {
                tracing::error!("Failed to remove local track {}: {}", track_id, err);
            }
        });
        Ok(())
    })
}

pub fn init(id: String, url: String) {
    console_error_panic_hook::set_once();

    static TRACING_INIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !TRACING_INIT.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let config = tracing_wasm::WASMLayerConfigBuilder::new()
            .set_max_level(tracing::Level::INFO)
            .build();
        tracing_wasm::set_as_global_default_with_config(config);
    }

    tracing::info!("Initializing Mist Engine WASM bindings for {}", id);
    L0.with(|l0| l0.initialize(NodeId(id), url));
}

pub fn update_position(x: f32, y: f32, z: f32) {
    ENGINE.with(|e| {
        let self_id = e.self_id.lock().unwrap().clone();
        {
            let mut store = e.node_store.lock().unwrap();
            store.nodes.insert(
                self_id.clone(),
                mistlib_core::overlay::node_store::NodeInfo {
                    id: self_id.clone(),
                    position: mistlib_core::overlay::dnve3::Vector3::new(x, y, z),
                },
            );
            store
                .last_updated
                .insert(self_id.clone(), web_time::Instant::now());
        }

        let ctx = {
            let state_lock = e.state.lock().unwrap();
            if let EngineState::Running(ctx) = &*state_lock {
                Some(ctx.clone())
            } else {
                None
            }
        };

        if let Some(ctx) = ctx {
            if let Some(ov) = &ctx.overlay {
                {
                    let mut rt = ov.routing_table.lock().unwrap();
                    rt.add_routing(self_id.clone(), self_id.clone(), &self_id);
                }

                let connected_nodes = ctx.transport.get_connected_nodes();
                if !connected_nodes.is_empty() {
                    let payload = serde_json::json!({
                        "type": "sync_pos",
                        "position": {"x": x, "y": y, "z": z},
                        "nodeId": self_id.0
                    })
                    .to_string();

                    use mistlib_core::signaling::{
                        MessageContent, OverlayMessage, SignalingEnvelope,
                    };
                    let envelope = SignalingEnvelope {
                        from: self_id.clone(),
                        to: NodeId("".to_string()),
                        hop_count: 1,
                        content: MessageContent::Overlay(OverlayMessage {
                            message_type: 100,
                            payload: payload.into_bytes(),
                        }),
                    };

                    if let Ok(data) = bincode::serialize(&envelope) {
                        for target in connected_nodes {
                            use mistlib_core::overlay::ActionHandler;
                            e.handle_action(mistlib_core::action::OverlayAction::SendMessage {
                                to: target,
                                data: bytes::Bytes::from(data.clone()),
                                method: DeliveryMethod::Unreliable,
                            });
                        }
                    }
                }
            }
        }
    });
}

pub fn get_neighbors() -> String {
    let connected_nodes = ENGINE.with(|e| {
        if let Ok(state) = e.state.try_lock() {
            if let EngineState::Running(ctx) = &*state {
                ctx.transport.get_connected_nodes()
            } else {
                vec![]
            }
        } else {
            vec![]
        }
    });

    ENGINE.with(|e| {
        if let Ok(store) = e.node_store.try_lock() {
            let connected_set: std::collections::HashSet<_> = connected_nodes.into_iter().collect();
            store.get_connected_nodes_json(&connected_set)
        } else {
            "[]".to_string()
        }
    })
}

pub fn get_all_nodes() -> String {
    let connected_nodes = ENGINE.with(|e| {
        if let Ok(state) = e.state.try_lock() {
            if let EngineState::Running(ctx) = &*state {
                ctx.transport.get_connected_nodes()
            } else {
                vec![]
            }
        } else {
            vec![]
        }
    });

    ENGINE.with(|e| {
        if let Ok(store) = e.node_store.try_lock() {
            let connected_set: std::collections::HashSet<_> = connected_nodes.into_iter().collect();
            store.get_all_nodes_json(&connected_set)
        } else {
            "[]".to_string()
        }
    })
}

pub fn join_room(room_id: String) {
    L0.with(|l0| l0.join_room(room_id));
}

pub fn leave_room() {
    L0.with(|l0| l0.leave_room());
}

pub fn set_config(data: String) -> bool {
    L0.with(|l0| {
        if let Ok(config) = serde_json::from_str::<mistlib_core::config::Config>(&data) {
            l0.set_config(config);
            true
        } else {
            
            ENGINE.with(|e| {
                let mut lock = e.config.lock().unwrap();
                lock.update_from_json(&data)
            })
        }
    })
}

pub fn send_message(target_id: String, data: &[u8], method: u32) {
    let payload = data.to_vec();
    let target = NodeId(target_id);
    let delivery = match method {
        DELIVERY_RELIABLE => DeliveryMethod::ReliableOrdered,
        DELIVERY_UNRELIABLE_ORDERED => DeliveryMethod::UnreliableOrdered,
        _ => DeliveryMethod::Unreliable,
    };

    wasm_bindgen_futures::spawn_local(async move {
        let ctx = ENGINE.with(|e| {
            let state = e.state.lock().unwrap();
            if let EngineState::Running(ctx) = &*state {
                Some(ctx.clone())
            } else {
                None
            }
        });

        if let Some(ctx) = ctx {
            let bytes = Bytes::from(payload);
            if target.0.is_empty() {
                let _ = ctx.transport.broadcast(bytes, delivery).await;
            } else {
                let _ = ctx.transport.send(&target, bytes, delivery).await;
            }
        }
    });
}

pub fn get_config() -> String {
    ENGINE.with(|e| {
        let config = e.config.lock().unwrap().clone();
        config.to_json_string()
    })
}

pub fn get_stats() -> String {
    let snapshot = mistlib_core::stats::STATS.snapshot_and_reset();
    let rtt_millis: std::collections::HashMap<String, f32> = snapshot
        .rtt_millis
        .iter()
        .map(|(k, v)| (k.0.clone(), *v))
        .collect();

    let stats = serde_json::json!({
        "messageCount": snapshot.message_count,
        "sendBits": snapshot.send_bits,
        "receiveBits": snapshot.receive_bits,
        "rttMillis": rtt_millis,
        "evalSendBits": snapshot.eval_send_bits,
        "evalReceiveBits": snapshot.eval_receive_bits,
        "evalMessageCount": snapshot.eval_message_count,
        "nodes": []
    });
    serde_json::to_string(&stats).unwrap_or_else(|_| "{}".to_string())
}
