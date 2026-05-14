use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::stats::STATS;
use mistlib_core::transport::{NetworkEvent, NetworkEventHandler};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    MediaStream, MessageEvent, RtcDataChannel, RtcPeerConnection, RtcPeerConnectionIceEvent,
    RtcTrackEvent,
};

pub struct Peer {
    pub pc: RtcPeerConnection,
    pub channels: Arc<RwLock<HashMap<DeliveryMethod, RtcDataChannel>>>,
}

impl Peer {
    pub fn new(pc: RtcPeerConnection) -> Self {
        Self {
            pc,
            channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn close_all(&self) {
        self.detach_peer_handlers();

        let channels = {
            let mut lock = self.channels.write().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *lock)
        };

        for (_, dc) in channels {
            Self::detach_data_channel_handlers(&dc);
            dc.close();
        }

        self.pc.close();
    }

    fn detach_peer_handlers(&self) {
        self.pc.set_oniceconnectionstatechange(None);
        self.pc.set_onicecandidate(None);
        self.pc.set_ondatachannel(None);
        self.pc.set_ontrack(None);
    }

    fn detach_data_channel_handlers(dc: &RtcDataChannel) {
        dc.set_onopen(None);
        dc.set_onclose(None);
        dc.set_onmessage(None);
    }

    pub fn setup_handlers(
        self: &Arc<Self>,
        remote_id: NodeId,
        signaler: Arc<dyn Signaler>,
        local_id: NodeId,
        room_id: String,
        connection_states: Arc<RwLock<HashMap<NodeId, ConnectionState>>>,
        event_handler: Arc<Mutex<Option<Arc<dyn NetworkEventHandler>>>>,
        peers: Arc<RwLock<HashMap<NodeId, Arc<Peer>>>>,
        peer_senders: Arc<RwLock<HashMap<NodeId, HashMap<String, web_sys::RtcRtpSender>>>>,
    ) {
        let conn_states = connection_states.clone();
        let remote_id_state = remote_id.clone();
        let peer_state = self.clone();
        let peers_state = peers.clone();
        let senders_state = peer_senders.clone();
        let onstatechange = Closure::wrap(Box::new(move |_ev: web_sys::Event| {
            let state = peer_state.pc.ice_connection_state();
            tracing::info!(
                "ICE Connection state to {} changed to: {:?}",
                remote_id_state.0,
                state
            );
            let mut states = conn_states.write().unwrap_or_else(|e| e.into_inner());
            match state {
                web_sys::RtcIceConnectionState::Connected
                | web_sys::RtcIceConnectionState::Completed => {
                    states.insert(remote_id_state.clone(), ConnectionState::Connecting);
                }
                web_sys::RtcIceConnectionState::Disconnected => {
                    states.insert(remote_id_state.clone(), ConnectionState::Connecting);
                    tracing::warn!(
                        "ICE disconnected for {}. keeping peer for recovery.",
                        remote_id_state.0
                    );
                }
                web_sys::RtcIceConnectionState::Failed | web_sys::RtcIceConnectionState::Closed => {
                    states.insert(remote_id_state.clone(), ConnectionState::Disconnected);
                    {
                        let mut peers = peers_state.write().unwrap_or_else(|e| e.into_inner());
                        peers.remove(&remote_id_state);
                    }
                    {
                        let mut senders = senders_state.write().unwrap_or_else(|e| e.into_inner());
                        senders.remove(&remote_id_state);
                    }
                }
                web_sys::RtcIceConnectionState::Checking => {
                    states.insert(remote_id_state.clone(), ConnectionState::Connecting);
                }
                _ => {}
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        self.pc
            .set_oniceconnectionstatechange(Some(onstatechange.as_ref().unchecked_ref()));
        onstatechange.forget();

        let signaler_cb = signaler.clone();
        let local_id_cb = local_id.clone();
        let remote_id_cand = remote_id.clone();
        let room_id_cb = room_id.clone();

        let onicecandidate = Closure::wrap(Box::new(move |ev: RtcPeerConnectionIceEvent| {
            if let Some(candidate) = ev.candidate() {
                let signaler = signaler_cb.clone();
                let local_id = local_id_cb.clone();
                let remote_id = remote_id_cand.clone();
                let room_id = room_id_cb.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let cand_json = candidate.to_json();
                    let cand_str = js_sys::JSON::stringify(&cand_json)
                        .unwrap_or_default()
                        .as_string()
                        .unwrap_or_default();
                    let _ = signaler
                        .send_signaling(
                            &remote_id,
                            MessageContent::Data(SignalingData {
                                sender_id: local_id,
                                receiver_id: remote_id.clone(),
                                room_id: room_id.clone(),
                                data: cand_str,
                                signaling_type: SignalingType::Candidate,
                            }),
                        )
                        .await;
                });
            }
        }) as Box<dyn FnMut(RtcPeerConnectionIceEvent)>);
        self.pc
            .set_onicecandidate(Some(onicecandidate.as_ref().unchecked_ref()));
        onicecandidate.forget();

        let peer_dc = self.clone();
        let event_handler_dc = event_handler.clone();
        let remote_id_dc = remote_id.clone();
        let connection_states_dc = connection_states.clone();
        let peers_dc = peers.clone();
        let peer_senders_dc = peer_senders.clone();
        let ondatachannel = Closure::wrap(Box::new(move |ev: web_sys::RtcDataChannelEvent| {
            let dc = ev.channel();
            let label = dc.label();
            let method = match label.as_str() {
                "reliable" => DeliveryMethod::ReliableOrdered,
                "unreliable-ordered" => DeliveryMethod::UnreliableOrdered,
                "unreliable" => DeliveryMethod::Unreliable,
                _ => DeliveryMethod::ReliableOrdered,
            };
            {
                let mut channels = peer_dc.channels.write().unwrap_or_else(|e| e.into_inner());
                channels.insert(method, dc.clone());
            }
            Peer::setup_dc_handlers(
                dc,
                event_handler_dc.clone(),
                remote_id_dc.clone(),
                connection_states_dc.clone(),
                peers_dc.clone(),
                peer_senders_dc.clone(),
            );
        })
            as Box<dyn FnMut(web_sys::RtcDataChannelEvent)>);
        self.pc
            .set_ondatachannel(Some(ondatachannel.as_ref().unchecked_ref()));
        ondatachannel.forget();

        let remote_id_track = remote_id.clone();
        let ontrack = Closure::wrap(Box::new(move |ev: RtcTrackEvent| {
            let track = ev.track();
            let track_id = track.id();
            let kind = track.kind();
            let streams = ev.streams();
            let stream = streams.get(0).dyn_into::<MediaStream>().ok();

            crate::app::emit_media_track_added(
                remote_id_track.clone(),
                track_id.clone(),
                kind.clone(),
                track.clone(),
                stream,
            );

            let remote_id_ended = remote_id_track.clone();
            let track_id_ended = track_id.clone();
            let kind_ended = kind.clone();
            let onended = Closure::wrap(Box::new(move |_ev: web_sys::Event| {
                crate::app::emit_media_track_removed(
                    remote_id_ended.clone(),
                    track_id_ended.clone(),
                    kind_ended.clone(),
                );
            }) as Box<dyn FnMut(web_sys::Event)>);
            track.set_onended(Some(onended.as_ref().unchecked_ref()));
            onended.forget();
        }) as Box<dyn FnMut(RtcTrackEvent)>);
        self.pc.set_ontrack(Some(ontrack.as_ref().unchecked_ref()));
        ontrack.forget();
    }

    pub fn setup_dc_handlers(
        dc: RtcDataChannel,
        handler: Arc<Mutex<Option<Arc<dyn NetworkEventHandler>>>>,
        from: NodeId,
        connection_states: Arc<RwLock<HashMap<NodeId, ConnectionState>>>,
        peers: Arc<RwLock<HashMap<NodeId, Arc<Peer>>>>,
        peer_senders: Arc<RwLock<HashMap<NodeId, HashMap<String, web_sys::RtcRtpSender>>>>,
    ) {
        dc.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);

        let label = dc.label().to_string();
        let from_msg = from.clone();
        let states_open = connection_states.clone();
        let onopen = Closure::wrap(Box::new(move |_ev: web_sys::Event| {
            tracing::info!("DataChannel {} to {} opened", label, from_msg.0);
            let mut lock = states_open.write().unwrap_or_else(|e| e.into_inner());
            let prev = lock.insert(from_msg.clone(), ConnectionState::Connected);

            if prev != Some(ConnectionState::Connected) {
                drop(lock);
                crate::app::emit_peer_connected(from_msg.clone());
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        dc.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();

        let from_close = from.clone();
        let from_close_for_cleanup = from.clone();
        let states_close = connection_states.clone();
        let peers_close = peers.clone();
        let senders_close = peer_senders.clone();
        let onclose = Closure::wrap(Box::new(move |_ev: web_sys::Event| {
            tracing::warn!(
                "DataChannel to {} closed, triggering immediate disconnect.",
                from_close.0
            );

            let peer = {
                let mut lock = peers_close.write().unwrap_or_else(|e| e.into_inner());
                lock.remove(&from_close_for_cleanup)
            };

            let Some(peer) = peer else { return };

            {
                let mut lock = states_close.write().unwrap_or_else(|e| e.into_inner());
                lock.remove(&from_close);
            }
            {
                let mut lock = senders_close.write().unwrap_or_else(|e| e.into_inner());
                lock.remove(&from_close_for_cleanup);
            }
            peer.close_all();
            // peerが確実に1回だけ取り出せた場合のみ切断通知を送る（重複防止済み）
            crate::app::emit_peer_disconnected(from_close.clone());
        }) as Box<dyn FnMut(web_sys::Event)>);
        dc.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();

        let onmessage = Closure::wrap(Box::new(move |ev: MessageEvent| {
            if let Ok(ab) = ev.data().dyn_into::<js_sys::ArrayBuffer>() {
                let array = js_sys::Uint8Array::new(&ab);
                let vec = array.to_vec();
                STATS.add_receive(vec.len() as u64);
                STATS.add_world_receive_frame(&vec);
                // Clone the Arc out of the mutex in a short-lived scope so the
                // lock is released before on_event is called.  Calling on_event
                // while still holding the lock can trigger re-entrant callbacks
                // that try to acquire the same mutex, causing the WASM
                // no_threads mutex to panic with "cannot recursively acquire
                // mutex".
                let maybe_h = {
                    let lock = handler.lock().unwrap_or_else(|e| e.into_inner());
                    lock.as_ref().cloned()
                };
                if let Some(h) = maybe_h {
                    h.on_event(NetworkEvent {
                        from: from.clone(),
                        data: bytes::Bytes::from(vec),
                    });
                } else {
                    tracing::warn!(
                        "DataChannel message from {} dropped - no handler registered",
                        from.0
                    );
                }
            } else {
                tracing::warn!(
                    "Received DataChannel message from {} but it's not an ArrayBuffer",
                    from.0
                );
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        dc.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();
    }
}
