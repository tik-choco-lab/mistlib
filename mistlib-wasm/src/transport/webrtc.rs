use async_trait::async_trait;
use bytes::Bytes;
use js_sys::Reflect;
use mistlib_core::signaling::{
    MessageContent, Signaler, SignalingData, SignalingHandler, SignalingType,
};
use mistlib_core::stats::STATS;
use mistlib_core::transport::{NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MediaStream, MediaStreamTrack, RtcConfiguration, RtcDataChannelInit, RtcIceCandidateInit,
    RtcPeerConnection, RtcRtpSender, RtcSdpType, RtcSessionDescriptionInit, RtcSignalingState,
};

pub mod peer;
pub use peer::Peer;

#[derive(Clone)]
pub struct LocalTrack {
    pub track: MediaStreamTrack,
    pub kind: String,
    pub published: bool,
}

pub struct WasmWebRtcTransport {
    pub signaler: Arc<dyn Signaler>,
    pub local_node_id: NodeId,
    pub peers: Arc<RwLock<HashMap<NodeId, Arc<Peer>>>>,
    pub event_handler: Arc<Mutex<Option<Arc<dyn NetworkEventHandler>>>>,
    pub connection_states: Arc<RwLock<HashMap<NodeId, ConnectionState>>>,
    pub room_id: Arc<RwLock<String>>,
    pub max_connections: AtomicU32,
    pub local_tracks: Arc<RwLock<HashMap<String, LocalTrack>>>,
    pub peer_senders: Arc<RwLock<HashMap<NodeId, HashMap<String, RtcRtpSender>>>>,
}

impl WasmWebRtcTransport {
    pub fn new(signaler: Arc<dyn Signaler>, local_node_id: NodeId) -> Self {
        Self {
            signaler,
            local_node_id,
            peers: Arc::new(RwLock::new(HashMap::new())),
            event_handler: Arc::new(Mutex::new(None)),
            connection_states: Arc::new(RwLock::new(HashMap::new())),
            room_id: Arc::new(RwLock::new("lobby".to_string())),
            max_connections: AtomicU32::new(30),
            local_tracks: Arc::new(RwLock::new(HashMap::new())),
            peer_senders: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn set_room_id(&self, room_id: String) {
        let mut lock = self.room_id.write().unwrap_or_else(|e| e.into_inner());
        *lock = room_id;
    }

    pub fn set_max_connections(&self, max: u32) {
        self.max_connections.store(max, Ordering::Relaxed);
    }

    pub async fn request_peers(&self) -> mistlib_core::error::Result<()> {
        let room_id = self
            .room_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.signaler
            .send_signaling(
                &NodeId("server".to_string()),
                MessageContent::Data(SignalingData {
                    sender_id: self.local_node_id.clone(),
                    receiver_id: NodeId("".to_string()),
                    room_id,
                    data: "".to_string(),
                    signaling_type: SignalingType::Request,
                }),
            )
            .await
    }

    fn create_pc(&self, remote_id: NodeId) -> Result<Arc<Peer>, JsValue> {
        let config = RtcConfiguration::new();
        let ice_server = web_sys::RtcIceServer::new();
        let urls = js_sys::Array::new();
        urls.push(&JsValue::from_str("stun:stun.l.google.com:19302"));
        ice_server.set_urls(&urls);

        let ice_servers = js_sys::Array::new();
        ice_servers.push(&ice_server);
        config.set_ice_servers(&ice_servers);

        let pc = RtcPeerConnection::new_with_configuration(&config)?;
        let peer = Arc::new(Peer::new(pc));

        peer.setup_handlers(
            remote_id.clone(),
            self.signaler.clone(),
            self.local_node_id.clone(),
            self.room_id
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            self.connection_states.clone(),
            self.event_handler.clone(),
        );

        let _ = self.attach_published_tracks_to_peer(&remote_id, &peer)?;

        Ok(peer)
    }

    fn attach_published_tracks_to_peer(
        &self,
        remote_id: &NodeId,
        peer: &Arc<Peer>,
    ) -> Result<bool, JsValue> {
        let tracks: Vec<(String, MediaStreamTrack)> = {
            let lock = self.local_tracks.read().unwrap_or_else(|e| e.into_inner());
            lock.iter()
                .filter(|(_, local)| local.published)
                .map(|(track_id, local)| (track_id.clone(), local.track.clone()))
                .collect()
        };

        let mut changed = false;
        let mut senders_lock = self.peer_senders.write().unwrap_or_else(|e| e.into_inner());
        let peer_senders = senders_lock.entry(remote_id.clone()).or_default();

        for (track_id, track) in tracks {
            if peer_senders.contains_key(&track_id) {
                continue;
            }
            let stream = MediaStream::new()?;
            stream.add_track(&track);
            let sender = peer.pc.add_track_0(&track, &stream);
            peer_senders.insert(track_id, sender);
            changed = true;
        }

        Ok(changed)
    }

    fn replace_track_for_existing_senders(&self, track_id: &str, track: &MediaStreamTrack) {
        let replacements: Vec<(NodeId, RtcRtpSender)> = self
            .peer_senders
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(node_id, senders)| {
                senders
                    .get(track_id)
                    .cloned()
                    .map(|sender| (node_id.clone(), sender))
            })
            .collect();

        for (node_id, sender) in replacements {
            let track = track.clone();
            let track_id = track_id.to_string();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(err) = JsFuture::from(sender.replace_track(Some(&track))).await {
                    tracing::error!(
                        "Failed to replace local track {} for peer {}: {:?}",
                        track_id,
                        node_id.0,
                        err
                    );
                }
            });
        }
    }

    async fn renegotiate_peer(
        &self,
        remote_id: &NodeId,
        peer: &Arc<Peer>,
    ) -> mistlib_core::error::Result<()> {
        if peer.pc.signaling_state() != RtcSignalingState::Stable {
            tracing::debug!(
                "Skipping renegotiation with {} because signaling state is not stable",
                remote_id.0
            );
            return Ok(());
        }

        let offer = JsFuture::from(peer.pc.create_offer())
            .await
            .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;
        let sdp = Reflect::get(&offer, &JsValue::from_str("sdp"))
            .map_err(|_| {
                mistlib_core::error::MistError::Internal("No SDP field in offer".to_string())
            })?
            .as_string()
            .ok_or_else(|| {
                mistlib_core::error::MistError::Internal("SDP is not a string".to_string())
            })?;

        let sdp_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        sdp_init.set_sdp(&sdp);
        JsFuture::from(peer.pc.set_local_description(&sdp_init))
            .await
            .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;

        let room_id = self
            .room_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.signaler
            .send_signaling(
                remote_id,
                MessageContent::Data(SignalingData {
                    sender_id: self.local_node_id.clone(),
                    receiver_id: remote_id.clone(),
                    room_id,
                    data: sdp,
                    signaling_type: SignalingType::Offer,
                }),
            )
            .await
    }

    pub fn register_local_track(
        &self,
        track_id: String,
        track: MediaStreamTrack,
    ) -> mistlib_core::error::Result<()> {
        let kind = track.kind();
        let published = {
            let mut lock = self.local_tracks.write().unwrap_or_else(|e| e.into_inner());
            let published = lock.get(&track_id).map(|entry| entry.published).unwrap_or(false);
            lock.insert(
                track_id.clone(),
                LocalTrack {
                    track: track.clone(),
                    kind,
                    published,
                },
            );
            published
        };

        if published {
            self.replace_track_for_existing_senders(&track_id, &track);
        }

        Ok(())
    }

    pub async fn publish_local_track(
        &self,
        track_id: &str,
    ) -> mistlib_core::error::Result<()> {
        {
            let mut lock = self.local_tracks.write().unwrap_or_else(|e| e.into_inner());
            let entry = lock.get_mut(track_id).ok_or_else(|| {
                mistlib_core::error::MistError::Internal(format!(
                    "Unknown local track: {}",
                    track_id
                ))
            })?;
            entry.published = true;
        }

        let peers: Vec<(NodeId, Arc<Peer>)> = self
            .peers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(node_id, peer)| (node_id.clone(), peer.clone()))
            .collect();

        for (node_id, peer) in peers {
            let changed = self
                .attach_published_tracks_to_peer(&node_id, &peer)
                .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;
            if changed {
                self.renegotiate_peer(&node_id, &peer).await?;
            }
        }

        Ok(())
    }

    pub async fn unpublish_local_track(
        &self,
        track_id: &str,
    ) -> mistlib_core::error::Result<()> {
        {
            let mut lock = self.local_tracks.write().unwrap_or_else(|e| e.into_inner());
            let entry = lock.get_mut(track_id).ok_or_else(|| {
                mistlib_core::error::MistError::Internal(format!(
                    "Unknown local track: {}",
                    track_id
                ))
            })?;
            entry.published = false;
        }

        let peers: Vec<(NodeId, Arc<Peer>)> = self
            .peers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(node_id, peer)| (node_id.clone(), peer.clone()))
            .collect();

        for (node_id, peer) in peers {
            let sender = {
                let mut senders = self.peer_senders.write().unwrap_or_else(|e| e.into_inner());
                senders
                    .get_mut(&node_id)
                    .and_then(|peer_senders| peer_senders.remove(track_id))
            };

            if let Some(sender) = sender {
                peer.pc.remove_track(&sender);
                self.renegotiate_peer(&node_id, &peer).await?;
            }
        }

        Ok(())
    }

    pub async fn remove_local_track(
        &self,
        track_id: &str,
    ) -> mistlib_core::error::Result<()> {
        let _ = self.unpublish_local_track(track_id).await;
        let mut lock = self.local_tracks.write().unwrap_or_else(|e| e.into_inner());
        lock.remove(track_id);
        Ok(())
    }

    pub fn set_local_track_enabled(
        &self,
        track_id: &str,
        enabled: bool,
    ) -> mistlib_core::error::Result<()> {
        let lock = self.local_tracks.read().unwrap_or_else(|e| e.into_inner());
        let entry = lock.get(track_id).ok_or_else(|| {
            mistlib_core::error::MistError::Internal(format!("Unknown local track: {}", track_id))
        })?;
        entry.track.set_enabled(enabled);
        Ok(())
    }

    pub fn get_local_track(&self, track_id: &str) -> Option<MediaStreamTrack> {
        self.local_tracks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(track_id)
            .map(|entry| entry.track.clone())
    }

    pub fn get_active_connection_states(&self) -> Vec<(NodeId, ConnectionState)> {
        let states = self
            .connection_states
            .read()
            .unwrap_or_else(|e| e.into_inner());
        states
            .iter()
            .filter(|(_, &s)| s == ConnectionState::Connected || s == ConnectionState::Connecting)
            .map(|(id, &s)| (id.clone(), s))
            .collect()
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Transport for WasmWebRtcTransport {
    async fn start(
        &self,
        handler: Arc<dyn NetworkEventHandler>,
    ) -> mistlib_core::error::Result<()> {
        {
            let mut lock = self.event_handler.lock().unwrap_or_else(|e| e.into_inner());
            *lock = Some(handler);
        }

        let room_id = self
            .room_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        tracing::info!(
            "Starting WasmWebRtcTransport. Sending Request to server in room: {}",
            room_id
        );
        self.request_peers().await?;

        Ok(())
    }

    async fn send(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> mistlib_core::error::Result<()> {
        let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
        if let Some(peer) = peers.get(node) {
            let channels = peer.channels.read().unwrap_or_else(|e| e.into_inner());
            if let Some(dc) = channels.get(&method) {
                if dc.ready_state() == web_sys::RtcDataChannelState::Open {
                    dc.send_with_u8_array(&data).map_err(|e| {
                        mistlib_core::error::MistError::Internal(format!("{:?}", e))
                    })?;
                    STATS.add_send(data.len() as u64);
                    return Ok(());
                }
            }
        }
        Err(mistlib_core::error::MistError::Internal(
            "Not connected".to_string(),
        ))
    }

    async fn broadcast(
        &self,
        data: Bytes,
        method: DeliveryMethod,
    ) -> mistlib_core::error::Result<()> {
        let nodes = self.get_connected_nodes();
        for node in nodes {
            let _ = self.send(&node, data.clone(), method).await;
        }
        Ok(())
    }

    fn get_connection_state(&self, node: &NodeId) -> ConnectionState {
        self.connection_states
            .read()
            .unwrap()
            .get(node)
            .cloned()
            .unwrap_or(ConnectionState::Disconnected)
    }

    async fn connect(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        tracing::info!("Connecting to peer: {}", node.0);

        {
            let states = self
                .connection_states
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if states.contains_key(node) {
                return Ok(());
            }

            let max = self.max_connections.load(Ordering::Relaxed) as usize;
            let current = states
                .values()
                .filter(|&&s| s == ConnectionState::Connected || s == ConnectionState::Connecting)
                .count();
            if current >= max {
                return Ok(());
            }
        }

        {
            let mut states = self
                .connection_states
                .write()
                .unwrap_or_else(|e| e.into_inner());
            states.insert(node.clone(), ConnectionState::Connecting);
        }

        let peer = self
            .create_pc(node.clone())
            .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;

        let dc_init = RtcDataChannelInit::new();
        dc_init.set_ordered(true);
        let reliable = peer
            .pc
            .create_data_channel_with_data_channel_dict("reliable", &dc_init);
        {
            let mut channels = peer.channels.write().unwrap_or_else(|e| e.into_inner());
            channels.insert(DeliveryMethod::ReliableOrdered, reliable.clone());
        }
        Peer::setup_dc_handlers(reliable, self.event_handler.clone(), node.clone());

        let unreliable_init = RtcDataChannelInit::new();
        unreliable_init.set_ordered(false);
        unreliable_init.set_max_retransmits(0);
        let unreliable = peer
            .pc
            .create_data_channel_with_data_channel_dict("unreliable", &unreliable_init);
        {
            let mut channels = peer.channels.write().unwrap_or_else(|e| e.into_inner());
            channels.insert(DeliveryMethod::Unreliable, unreliable.clone());
        }
        Peer::setup_dc_handlers(unreliable, self.event_handler.clone(), node.clone());

        {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            peers.insert(node.clone(), peer.clone());
        }

        self.renegotiate_peer(node, &peer).await?;

        Ok(())
    }

    async fn disconnect(&self, node: &NodeId) -> mistlib_core::error::Result<()> {
        let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
        if let Some(peer) = peers.remove(node) {
            peer.pc.close();
        }
        let mut senders = self.peer_senders.write().unwrap_or_else(|e| e.into_inner());
        senders.remove(node);
        Ok(())
    }

    fn get_connected_nodes(&self) -> Vec<NodeId> {
        let states = self
            .connection_states
            .read()
            .unwrap_or_else(|e| e.into_inner());
        states
            .iter()
            .filter(|(_, &s)| s == ConnectionState::Connected)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl SignalingHandler for WasmWebRtcTransport {
    async fn handle_message(&self, msg: MessageContent) -> mistlib_core::error::Result<()> {
        let data = match msg {
            MessageContent::Data(d) => d,
            _ => return Ok(()),
        };

        match data.signaling_type {
            SignalingType::Offer => {
                tracing::info!("Received Offer from: {}", data.sender_id.0);
                let peer = {
                    let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
                    peers.get(&data.sender_id).cloned()
                };
                let peer = match peer {
                    Some(peer) => peer,
                    None => self
                        .create_pc(data.sender_id.clone())
                        .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?,
                };
                let sdp_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
                sdp_init.set_sdp(&data.data);
                JsFuture::from(peer.pc.set_remote_description(&sdp_init))
                    .await
                    .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;

                let answer = JsFuture::from(peer.pc.create_answer())
                    .await
                    .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;
                let answer_sdp = Reflect::get(&answer, &JsValue::from_str("sdp"))
                    .map_err(|_| {
                        mistlib_core::error::MistError::Internal(
                            "No SDP field in answer".to_string(),
                        )
                    })?
                    .as_string()
                    .ok_or_else(|| {
                        mistlib_core::error::MistError::Internal("SDP is not a string".to_string())
                    })?;

                let answer_init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
                answer_init.set_sdp(&answer_sdp);
                JsFuture::from(peer.pc.set_local_description(&answer_init))
                    .await
                    .map_err(|e| mistlib_core::error::MistError::Internal(format!("{:?}", e)))?;

                let room_id = self
                    .room_id
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                self.signaler
                    .send_signaling(
                        &data.sender_id,
                        MessageContent::Data(SignalingData {
                            sender_id: self.local_node_id.clone(),
                            receiver_id: data.sender_id.clone(),
                            room_id,
                            data: answer_sdp,
                            signaling_type: SignalingType::Answer,
                        }),
                    )
                    .await?;

                let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
                peers.insert(data.sender_id.clone(), peer);
            }
            SignalingType::Answer => {
                tracing::info!("Received Answer from: {}", data.sender_id.0);
                let peer = {
                    let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
                    peers.get(&data.sender_id).cloned()
                };
                if let Some(peer) = peer {
                    let sdp_init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
                    sdp_init.set_sdp(&data.data);
                    JsFuture::from(peer.pc.set_remote_description(&sdp_init))
                        .await
                        .map_err(|e| {
                            mistlib_core::error::MistError::Internal(format!("{:?}", e))
                        })?;
                }
            }
            SignalingType::Candidate => {
                tracing::info!("Received Candidate from: {}", data.sender_id.0);
                let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
                if let Some(peer) = peers.get(&data.sender_id) {
                    if let Ok(cand_obj) = js_sys::JSON::parse(&data.data) {
                        let candidate_str =
                            Reflect::get(&cand_obj, &JsValue::from_str("candidate"))
                                .ok()
                                .and_then(|v| v.as_string())
                                .unwrap_or_default();
                        let sdp_mid = Reflect::get(&cand_obj, &JsValue::from_str("sdpMid"))
                            .ok()
                            .and_then(|v| v.as_string());
                        let sdp_m_line_index =
                            Reflect::get(&cand_obj, &JsValue::from_str("sdpMLineIndex"))
                                .ok()
                                .and_then(|v| v.as_f64())
                                .map(|v| v as u16);

                        let cand_init = RtcIceCandidateInit::new(&candidate_str);
                        if let Some(mid) = sdp_mid {
                            cand_init.set_sdp_mid(Some(&mid));
                        }
                        if let Some(m_line_index) = sdp_m_line_index {
                            cand_init.set_sdp_m_line_index(Some(m_line_index));
                        }
                        let _ = peer
                            .pc
                            .add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&cand_init));
                    }
                }
            }
            SignalingType::Candidates => {
                let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
                if let Some(peer) = peers.get(&data.sender_id) {
                    if let Ok(candidates) = serde_json::from_str::<Vec<String>>(&data.data) {
                        for cand in candidates {
                            if let Ok(cand_obj) = js_sys::JSON::parse(&cand) {
                                let candidate_str =
                                    Reflect::get(&cand_obj, &JsValue::from_str("candidate"))
                                        .ok()
                                        .and_then(|v| v.as_string())
                                        .unwrap_or_default();
                                let sdp_mid = Reflect::get(&cand_obj, &JsValue::from_str("sdpMid"))
                                    .ok()
                                    .and_then(|v| v.as_string());
                                let sdp_m_line_index =
                                    Reflect::get(&cand_obj, &JsValue::from_str("sdpMLineIndex"))
                                        .ok()
                                        .and_then(|v| v.as_f64())
                                        .map(|v| v as u16);

                                let cand_init = RtcIceCandidateInit::new(&candidate_str);
                                if let Some(mid) = sdp_mid {
                                    cand_init.set_sdp_mid(Some(&mid));
                                }
                                if let Some(m_line_index) = sdp_m_line_index {
                                    cand_init.set_sdp_m_line_index(Some(m_line_index));
                                }
                                let _ = peer.pc.add_ice_candidate_with_opt_rtc_ice_candidate_init(
                                    Some(&cand_init),
                                );
                            }
                        }
                    }
                }
            }
            SignalingType::Request => {
                tracing::info!("Received Request from: {}", data.sender_id.0);
                if self.local_node_id != data.sender_id && self.local_node_id.0 < data.sender_id.0 {
                    tracing::info!("Initiating connect to: {}", data.sender_id.0);
                    let _ = self.connect(&data.sender_id).await;
                }
            }
        }
        Ok(())
    }
}
