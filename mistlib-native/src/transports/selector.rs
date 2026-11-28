//! Per-peer transport selection layer (SPEC-25 O2).
//!
//! The engine holds a [`LinkSelector`] instead of a concrete
//! `WebRtcTransport`. The selector implements the same
//! [`Transport`] and [`SignalingHandler`] traits the WebRTC transport does,
//! and owns every transport-specific operation the engine needs
//! (send admission, media tracks, room announce, teardown). Each call is
//! routed to the link that carries the peer it is about, or fanned out to
//! every link for room-wide operations.
//!
//! O2.1 has a single link kind, WebRTC, so every call is a plain delegation
//! to the room's `WebRtcTransport`: behavior, timing and wire output are the
//! same as calling that transport directly. Adding a link (ICE+QUIC) means
//! adding a [`LinkKind`] / [`Link`] variant, the capability exchange behind
//! [`LinkTable::remote_caps`], and a rule in [`select_link`]; the compiler
//! then points at every `match` that must route the new variant. Upper
//! layers (overlay, DNVE3, storage, mistlib-core) do not change.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bytes::Bytes;
use mistlib_core::action::SendLane;
use mistlib_core::error::Result;
use mistlib_core::signaling::capabilities::{
    decode_connect_request_transports, encode_connect_request_data, TRANSPORT_ICE_QUIC_V1,
    TRANSPORT_WEBRTC,
};
use mistlib_core::signaling::{MessageContent, SignalingHandler, SignalingType};
use mistlib_core::transport::send::{ByteBudget, SendFuture, SendReservation};
use mistlib_core::transport::{NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};
use tokio::sync::mpsc;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

use super::ice_quic::IceQuicPeers;
use super::webrtc::{MediaTrackEvent, SendCommit, WebRtcTransport};

/// A kind of link a peer can be carried over. The declaration order is not
/// significant; [`select_link`] decides preference explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LinkKind {
    /// WebRTC (ICE + DTLS + SCTP DataChannels, SRTP media). The default for
    /// every pair and the only link that can reach a browser or a peer that
    /// announces no capabilities.
    WebRtc,
    /// Native↔native ICE + QUIC (`transports::ice_quic`, announced as `iq1`).
    /// Enabled by `webrtc.iceQuic` (default true); never chosen unless both sides
    /// announce it.
    IceQuic,
}

/// Whether the ICE+QUIC link is routed (SPEC-25 O2.2). Kept as a single
/// switch: with `false`, every peer is carried by WebRTC even when
/// [`select_link`] would pick ICE+QUIC.
pub(crate) const ICE_QUIC_LINK_WIRED: bool = true;

/// Process-wide switch, set from `Config::webrtc.ice_quic` when a session is
/// built (the native config is process-wide). Read by the signaling layer to
/// decide whether to announce `iq1`.
static ICE_QUIC_ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_ice_quic_enabled(enabled: bool) {
    ICE_QUIC_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn ice_quic_enabled() -> bool {
    ICE_QUIC_ENABLED.load(Ordering::Relaxed)
}

/// Link names this build announces: nothing at all unless ICE+QUIC is
/// enabled, so the default wire output is unchanged.
pub(crate) fn announced_transports() -> &'static [&'static str] {
    transports_for(ice_quic_enabled() && !legacy_media_only())
}

/// The app listens for WebRTC media tracks (`register_media_track_handler`)
/// but not for RTP over ICE+QUIC (`register_media_packet_handler`). Such an
/// app would hear nothing from a peer carried by ICE+QUIC, so this build
/// then neither announces nor selects ICE+QUIC and stays on WebRTC.
pub(crate) fn legacy_media_only() -> bool {
    let webrtc_media = super::webrtc::GLOBAL_MEDIA_TX
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    let packet_media = super::media_packet::GLOBAL_MEDIA_PACKET_TX
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    webrtc_media && !packet_media
}

const fn transports_for(ice_quic: bool) -> &'static [&'static str] {
    if ice_quic {
        &[TRANSPORT_WEBRTC, TRANSPORT_ICE_QUIC_V1]
    } else {
        &[]
    }
}

/// `SignalingData::data` for an outgoing CONNECT `Request`: the empty string
/// (exactly as before) unless ICE+QUIC is enabled.
pub(crate) fn connect_request_data() -> String {
    encode_connect_request_data(announced_transports())
}

impl LinkKind {
    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// The set of link kinds one side supports. An empty set means the side
/// announced nothing (an older peer, or no capability exchange yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct LinkCaps(u8);

impl LinkCaps {
    /// A side that announced no capabilities.
    pub(crate) const UNANNOUNCED: Self = Self(0);

    pub(crate) const fn of(kinds: &[LinkKind]) -> Self {
        let mut bits = 0;
        let mut i = 0;
        while i < kinds.len() {
            bits |= kinds[i].bit();
            i += 1;
        }
        Self(bits)
    }

    const fn supports(self, kind: LinkKind) -> bool {
        self.0 & kind.bit() != 0
    }

    /// Capabilities from announced link names; unknown names are ignored.
    pub(crate) fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut caps = Self::UNANNOUNCED;
        for name in names {
            let kind = match name {
                TRANSPORT_WEBRTC => LinkKind::WebRtc,
                TRANSPORT_ICE_QUIC_V1 => LinkKind::IceQuic,
                _ => continue,
            };
            caps.0 |= kind.bit();
        }
        caps
    }
}

/// What this native build can carry with `webrtc.iceQuic: false`.
pub(crate) const LOCAL_CAPS: LinkCaps = LinkCaps::of(&[LinkKind::WebRtc]);
/// What this native build can carry with `webrtc.iceQuic` enabled.
pub(crate) const LOCAL_CAPS_ICE_QUIC: LinkCaps =
    LinkCaps::of(&[LinkKind::WebRtc, LinkKind::IceQuic]);

/// Deterministic link choice for a pair. Both sides evaluate it with the
/// arguments swapped and must agree, so the rule has to be symmetric in
/// `local` / `remote`. A link other than WebRTC is chosen only when both
/// sides announce it; everything else, including a remote that announced
/// nothing, falls back to WebRTC (SPEC-25 §1, §6.2).
pub(crate) fn select_link(local: LinkCaps, remote: LinkCaps) -> LinkKind {
    if local.supports(LinkKind::IceQuic) && remote.supports(LinkKind::IceQuic) {
        LinkKind::IceQuic
    } else {
        LinkKind::WebRtc
    }
}

/// A borrowed handle to the link that carries one peer.
pub(crate) enum Link<'a> {
    WebRtc(&'a Arc<WebRtcTransport>),
    IceQuic(&'a Arc<IceQuicPeers>),
}

/// Per-peer link table: the room's link instances, and which one carries
/// each peer.
///
/// O2.1 keeps no per-peer rows. No peer announces capabilities yet, so every
/// peer is [`LinkCaps::UNANNOUNCED`] and resolves to WebRTC without a lookup
/// or a lock on the send path. The capability exchange adds the rows (remote
/// capabilities, and a pinned WebRTC fallback after a failed attempt) behind
/// [`Self::remote_caps`].
pub(crate) struct LinkTable {
    local: LinkCaps,
    webrtc: Arc<WebRtcTransport>,
    /// Capabilities each peer announced in a CONNECT `Request`. Only kept
    /// (and only consulted) when the local side supports a link other than
    /// WebRTC, so the default send path takes no lock.
    remote: RwLock<HashMap<NodeId, LinkCaps>>,
    /// The room's ICE+QUIC peers; `None` with `webrtc.iceQuic: false`.
    quic: Option<Arc<IceQuicPeers>>,
}

impl LinkTable {
    fn new(webrtc: Arc<WebRtcTransport>, local: LinkCaps) -> Self {
        Self {
            local,
            webrtc,
            remote: RwLock::new(HashMap::new()),
            quic: None,
        }
    }

    fn tracks_remote_caps(&self) -> bool {
        self.local != LOCAL_CAPS
    }

    fn remote_caps(&self, node: &NodeId) -> LinkCaps {
        if !self.tracks_remote_caps() {
            return LinkCaps::UNANNOUNCED;
        }
        self.remote
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(node)
            .copied()
            .unwrap_or(LinkCaps::UNANNOUNCED)
    }

    /// Records what `node` announced. A Request that announces nothing
    /// leaves earlier knowledge in place (restart markers and legacy empty
    /// Requests carry no capabilities).
    fn learn_remote_caps(&self, node: &NodeId, caps: LinkCaps) {
        if !self.tracks_remote_caps() || caps == LinkCaps::UNANNOUNCED {
            return;
        }
        self.remote
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(node.clone(), caps);
    }

    /// The link [`select_link`] chooses for `node`.
    pub(crate) fn selected_kind(&self, node: &NodeId) -> LinkKind {
        select_link(self.local, self.remote_caps(node))
    }

    /// The link a new connection to `node` should use: the selection rule,
    /// unless the link is not wired/configured or the pair is pinned to
    /// WebRTC after a failed ICE+QUIC setup.
    pub(crate) fn kind_for(&self, node: &NodeId) -> LinkKind {
        match (self.selected_kind(node), &self.quic) {
            (LinkKind::IceQuic, Some(quic))
                if ICE_QUIC_LINK_WIRED && !quic.pinned_to_webrtc(node) && !legacy_media_only() =>
            {
                LinkKind::IceQuic
            }
            _ => LinkKind::WebRtc,
        }
    }

    /// The link that carries `node` now: ICE+QUIC only once its link is
    /// established; everything else (including a pending ICE+QUIC attempt)
    /// is WebRTC.
    pub(crate) fn link_for(&self, node: &NodeId) -> Link<'_> {
        match &self.quic {
            Some(quic) if quic.carries(node) => Link::IceQuic(quic),
            _ => Link::WebRtc(&self.webrtc),
        }
    }
}

/// The per-room transport the engine holds (see the module docs).
///
/// Room-wide operations below go to every link; with WebRTC as the only
/// link they delegate to it directly. Per-peer operations go through
/// [`LinkTable::link_for`].
pub(crate) struct LinkSelector {
    links: LinkTable,
}

impl LinkSelector {
    pub(crate) fn new(webrtc: Arc<WebRtcTransport>) -> Self {
        let local = if ice_quic_enabled() {
            LOCAL_CAPS_ICE_QUIC
        } else {
            LOCAL_CAPS
        };
        Self {
            links: LinkTable::new(webrtc, local),
        }
    }

    /// Attach the room's ICE+QUIC peers (when `webrtc.iceQuic` is on). Their count
    /// joins WebRTC's for the room-wide connection limit.
    pub(crate) fn with_ice_quic(mut self, quic: Arc<IceQuicPeers>) -> Self {
        self.links.webrtc.set_external_active(quic.active_counter());
        self.links.quic = Some(quic);
        self
    }

    fn quic(&self) -> Option<&Arc<IceQuicPeers>> {
        self.links.quic.as_ref()
    }

    /// Learn the capabilities a signaling message announces. The engine
    /// calls this before it registers the sender as a known node: that
    /// registration can trigger an immediate overlay evaluation (and a
    /// connect), which must already see the sender's capabilities.
    /// `handle_message` learns them as well; learning twice is harmless.
    pub(crate) fn observe_signaling(&self, data: &mistlib_core::signaling::SignalingData) {
        if data.signaling_type == SignalingType::Request {
            if let Some(names) = decode_connect_request_transports(&data.data) {
                self.links.learn_remote_caps(
                    &data.sender_id,
                    LinkCaps::from_names(names.iter().map(String::as_str)),
                );
            }
        }
    }

    /// Test constructor with explicit local capabilities (the production
    /// constructor reads the process-wide switch).
    #[cfg(test)]
    fn with_local_caps(webrtc: Arc<WebRtcTransport>, local: LinkCaps) -> Self {
        Self {
            links: LinkTable::new(webrtc, local),
        }
    }

    fn link_for(&self, node: &NodeId) -> Link<'_> {
        self.links.link_for(node)
    }

    /// The WebRTC link itself, for WebRTC-specific diagnostics (SCTP and
    /// candidate counters in `engine/stats.rs`) and tests. Not a routing
    /// path: sends, connection management and media go through the
    /// selector's own methods.
    pub(crate) fn webrtc(&self) -> &Arc<WebRtcTransport> {
        &self.links.webrtc
    }

    // ---- per-peer send admission ----

    pub(crate) async fn reserve_overlay_send(
        &self,
        node: &NodeId,
        bytes: usize,
        lane: SendLane,
    ) -> Result<SendReservation> {
        match self.link_for(node) {
            Link::WebRtc(wt) => wt.reserve_overlay_send(node, bytes, lane).await,
            Link::IceQuic(q) => q.reserve_overlay_send(node, bytes).await,
        }
    }

    pub(crate) async fn enqueue_received_send(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> Result<()> {
        match self.link_for(node) {
            Link::WebRtc(wt) => wt.enqueue_received_send(node, data, method).await,
            Link::IceQuic(q) => q.enqueue(node, data, method).await,
        }
    }

    /// `node` may be the broadcast id; the WebRTC link fans that out over its
    /// own connected peers.
    pub(crate) fn retain_received_send(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> SendFuture<'static, Result<()>> {
        match self.link_for(node) {
            Link::WebRtc(wt) => wt.retain_received_send(node, data, method),
            Link::IceQuic(q) => q.retain(node, data, method),
        }
    }

    pub(crate) async fn send_wait_with(
        &self,
        node: &NodeId,
        build: impl FnOnce(&dyn Fn(usize) -> Result<()>, SendCommit<'_>) -> Result<()>,
    ) -> Result<()> {
        match self.link_for(node) {
            Link::WebRtc(wt) => wt.send_wait_with(node, build).await,
            Link::IceQuic(q) => q.send_wait_with(node, build).await,
        }
    }

    pub(crate) fn try_enqueue_send_with(
        &self,
        node: &NodeId,
        build: impl FnOnce(&dyn Fn(usize) -> Result<()>, SendCommit<'_>) -> Result<()>,
    ) -> Result<()> {
        match self.link_for(node) {
            Link::WebRtc(wt) => wt.try_enqueue_send_with(node, build),
            Link::IceQuic(q) => q.try_enqueue_with(node, build),
        }
    }

    pub(crate) fn try_enqueue_send(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> Result<()> {
        match self.link_for(node) {
            Link::WebRtc(wt) => wt.try_enqueue_send(node, data, method),
            Link::IceQuic(q) => q.try_enqueue(node, data, method),
        }
    }

    // ---- room-wide operations ----

    pub(crate) fn try_enqueue_broadcast(&self, data: Bytes, method: DeliveryMethod) -> Result<()> {
        if let Some(quic) = self.quic() {
            for node in quic.connected_nodes() {
                let _ = quic.try_enqueue(&node, data.clone(), method);
            }
        }
        self.links.webrtc.try_enqueue_broadcast(data, method)
    }

    /// Connected, connecting and reconnecting peers (the overlay's view of
    /// "active"). Note this is the WebRTC transport's inherent method, which
    /// is wider than `Transport::get_active_connection_states`'s default
    /// (connected only); the engine calls this one.
    pub(crate) fn get_active_connection_states(&self) -> Vec<(NodeId, ConnectionState)> {
        let mut states = self.links.webrtc.get_active_connection_states();
        self.merge_quic_states(&mut states);
        states
    }

    /// Peers the ICE+QUIC link holds (any state); empty without it.
    pub(crate) fn ice_quic_states(&self) -> Vec<(NodeId, ConnectionState)> {
        self.quic()
            .map(|quic| quic.active_states())
            .unwrap_or_default()
    }

    /// ICE+QUIC peers override WebRTC's entry for the same node.
    fn merge_quic_states(&self, states: &mut Vec<(NodeId, ConnectionState)>) {
        if let Some(quic) = self.quic() {
            let quic_states = quic.active_states();
            states.retain(|(node, _)| !quic_states.iter().any(|(n, _)| n == node));
            states.extend(quic_states);
        }
    }

    pub(crate) async fn announce_to_room(&self) -> Result<()> {
        self.links.webrtc.announce_to_room().await
    }

    pub(crate) async fn close_all_peer_connections(&self) {
        if let Some(quic) = self.quic() {
            quic.close_all();
        }
        self.links.webrtc.close_all_peer_connections().await;
    }

    pub(crate) fn stop_session_sweeper(&self) {
        self.links.webrtc.stop_session_sweeper();
    }

    // ---- media (WebRTC only; QUIC DATAGRAM media is SPEC-25 KR 2.5) ----

    pub(crate) fn set_media_track_handler(&self, tx: mpsc::UnboundedSender<MediaTrackEvent>) {
        self.links.webrtc.set_media_track_handler(tx);
    }

    pub(crate) async fn publish_local_track(
        &self,
        track: Arc<TrackLocalStaticRTP>,
    ) -> crate::error::Result<()> {
        self.links.webrtc.publish_local_track(track).await
    }

    pub(crate) async fn unpublish_local_track(
        &self,
        track: Arc<TrackLocalStaticRTP>,
    ) -> crate::error::Result<()> {
        self.links.webrtc.unpublish_local_track(track).await
    }

    pub(crate) async fn register_local_track(
        &self,
        track: Arc<TrackLocalStaticRTP>,
    ) -> crate::error::Result<()> {
        self.links.webrtc.register_local_track(track).await
    }

    pub(crate) async fn set_local_track_audience(
        &self,
        track_id: &str,
        nodes: Option<Vec<NodeId>>,
    ) -> crate::error::Result<()> {
        self.links
            .webrtc
            .set_local_track_audience(track_id, nodes)
            .await
    }

    pub(crate) async fn remove_local_track(&self, track_id: &str) -> crate::error::Result<()> {
        self.links.webrtc.remove_local_track(track_id).await
    }

    /// Write one RTP packet of a published local track to every peer in the
    /// track's audience: WebRTC peers through the track itself, ICE+QUIC
    /// peers as a QUIC DATAGRAM (SPEC-25 KR 2.5). Packets for a track that
    /// is registered but not published go nowhere, as with WebRTC.
    pub(crate) async fn write_local_rtp(
        &self,
        track_id: &str,
        packet: &webrtc::rtp::packet::Packet,
    ) -> crate::error::Result<()> {
        use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
        use webrtc::util::Marshal;
        let track = self
            .links
            .webrtc
            .published_tracks
            .read()
            .unwrap()
            .get(track_id)
            .cloned()
            .ok_or_else(|| {
                crate::error::MistError::Internal(format!("Unpublished local track: {track_id}"))
            })?;
        // WebRTC peers (no bound sender = nothing to do, not an error).
        let _ = track.write_rtp(packet).await;
        let Some(quic) = self.quic() else {
            return Ok(());
        };
        let targets: Vec<NodeId> = {
            let audiences = self.links.webrtc.registered_tracks.read().unwrap();
            quic.connected_nodes()
                .into_iter()
                .filter(|node| audiences.get(track_id).is_none_or(|a| a.allows(node)))
                .collect()
        };
        if targets.is_empty() {
            return Ok(());
        }
        let codec = track.codec();
        let desc = crate::transports::media_packet::TrackDescriptor {
            no: 0,
            id: track.id().to_string(),
            stream_id: track.stream_id().to_string(),
            kind: track.kind().to_string(),
            mime_type: codec.mime_type.clone(),
            clock_rate: codec.clock_rate,
            channels: codec.channels,
        };
        let rtp = packet
            .marshal()
            .map_err(|e| crate::error::MistError::Internal(format!("RTP marshal: {e}")))?;
        for node in targets {
            if let Err(error) = quic.send_rtp(&node, &desc, rtp.clone()).await {
                tracing::debug!("[LinkSelector] media to {} dropped: {}", node.0, error);
            }
        }
        Ok(())
    }
}

/// Every method is forwarded explicitly, including the ones `Transport`
/// gives a default body, so the selector always matches whatever the
/// WebRTC link implements.
#[async_trait]
impl Transport for LinkSelector {
    async fn start(&self, handler: Arc<dyn NetworkEventHandler>) -> Result<()> {
        if let Some(quic) = self.quic() {
            quic.set_handler(handler.clone());
        }
        <WebRtcTransport as Transport>::start(&self.links.webrtc, handler).await
    }

    async fn send(&self, node: &NodeId, data: Bytes, method: DeliveryMethod) -> Result<()> {
        match self.link_for(node) {
            Link::WebRtc(wt) => <WebRtcTransport as Transport>::send(wt, node, data, method).await,
            Link::IceQuic(q) => q.enqueue(node, data, method).await,
        }
    }

    async fn send_wait(&self, node: &NodeId, data: Bytes, method: DeliveryMethod) -> Result<()> {
        match self.link_for(node) {
            Link::WebRtc(wt) => {
                <WebRtcTransport as Transport>::send_wait(wt, node, data, method).await
            }
            Link::IceQuic(q) => q.send_wait(node, data, method).await,
        }
    }

    async fn send_response(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> Result<()> {
        match self.link_for(node) {
            Link::WebRtc(wt) => {
                <WebRtcTransport as Transport>::send_response(wt, node, data, method).await
            }
            Link::IceQuic(q) => q.send_wait(node, data, method).await,
        }
    }

    async fn send_in_lane(
        &self,
        node: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
        lane: SendLane,
        written: bool,
    ) -> Result<()> {
        match self.link_for(node) {
            Link::WebRtc(wt) => {
                <WebRtcTransport as Transport>::send_in_lane(wt, node, data, method, lane, written)
                    .await
            }
            Link::IceQuic(q) => {
                if written {
                    q.send_wait(node, data, method).await
                } else {
                    q.enqueue(node, data, method).await
                }
            }
        }
    }

    async fn broadcast(&self, data: Bytes, method: DeliveryMethod) -> Result<()> {
        if let Some(quic) = self.quic() {
            for node in quic.connected_nodes() {
                let _ = quic.enqueue(&node, data.clone(), method).await;
            }
        }
        <WebRtcTransport as Transport>::broadcast(&self.links.webrtc, data, method).await
    }

    fn get_connection_state(&self, node: &NodeId) -> ConnectionState {
        if let Some(state) = self.quic().and_then(|q| q.state(node)) {
            return state;
        }
        <WebRtcTransport as Transport>::get_connection_state(&self.links.webrtc, node)
    }

    fn is_reachable(&self, node: &NodeId) -> bool {
        match self.link_for(node) {
            Link::WebRtc(wt) => <WebRtcTransport as Transport>::is_reachable(wt, node),
            Link::IceQuic(q) => q.carries(node),
        }
    }

    async fn connect(&self, node: &NodeId) -> Result<()> {
        if let Some(quic) = self.quic() {
            if quic.state(node).is_some() {
                return Ok(()); // already connecting or connected over ICE+QUIC
            }
            if self.links.kind_for(node) == LinkKind::IceQuic {
                return quic.connect(node).await;
            }
        }
        <WebRtcTransport as Transport>::connect(&self.links.webrtc, node).await
    }

    async fn disconnect(&self, node: &NodeId) -> Result<()> {
        if let Some(quic) = self.quic() {
            if quic.state(node).is_some() {
                quic.disconnect(node);
                return Ok(());
            }
        }
        <WebRtcTransport as Transport>::disconnect(&self.links.webrtc, node).await
    }

    fn message_size_limit(&self, node: &NodeId) -> Option<u32> {
        match self.link_for(node) {
            Link::WebRtc(wt) => <WebRtcTransport as Transport>::message_size_limit(wt, node),
            Link::IceQuic(q) => q.message_size_limit(node),
        }
    }

    fn bulk_admission_budget(&self, node: &NodeId) -> Option<Arc<ByteBudget>> {
        match self.link_for(node) {
            Link::WebRtc(wt) => <WebRtcTransport as Transport>::bulk_admission_budget(wt, node),
            // QUIC runs its own congestion control; no SCTP bulk budget.
            Link::IceQuic(_) => None,
        }
    }

    fn send_generation(&self, node: &NodeId) -> Option<u64> {
        match self.link_for(node) {
            Link::WebRtc(wt) => <WebRtcTransport as Transport>::send_generation(wt, node),
            Link::IceQuic(_) => None,
        }
    }

    async fn suspect_disconnected(&self, node: &NodeId) -> Result<()> {
        match self.link_for(node) {
            Link::WebRtc(wt) => {
                <WebRtcTransport as Transport>::suspect_disconnected(wt, node).await
            }
            // QUIC's idle timeout detects a silent peer itself.
            Link::IceQuic(_) => Ok(()),
        }
    }

    async fn clear_suspect(&self, node: &NodeId) -> Result<()> {
        match self.link_for(node) {
            Link::WebRtc(wt) => <WebRtcTransport as Transport>::clear_suspect(wt, node).await,
            Link::IceQuic(_) => Ok(()),
        }
    }

    fn get_connected_nodes(&self) -> Vec<NodeId> {
        let mut nodes = <WebRtcTransport as Transport>::get_connected_nodes(&self.links.webrtc);
        if let Some(quic) = self.quic() {
            for node in quic.connected_nodes() {
                if !nodes.contains(&node) {
                    nodes.push(node);
                }
            }
        }
        nodes
    }

    fn get_active_connection_states(&self) -> Vec<(NodeId, ConnectionState)> {
        let mut states =
            <WebRtcTransport as Transport>::get_active_connection_states(&self.links.webrtc);
        self.merge_quic_states(&mut states);
        states
    }
}

/// Bootstrap and overlay-relayed signaling. Every message today is WebRTC
/// signaling (offer/answer/candidate/request). A second link adds its own
/// `SignalingType` values and is routed here by type.
#[async_trait]
impl SignalingHandler for LinkSelector {
    async fn handle_message(&self, msg: MessageContent) -> Result<()> {
        if let MessageContent::Data(data) = &msg {
            match data.signaling_type {
                SignalingType::Request => {
                    if let Some(names) = decode_connect_request_transports(&data.data) {
                        self.links.learn_remote_caps(
                            &data.sender_id,
                            LinkCaps::from_names(names.iter().map(String::as_str)),
                        );
                    }
                    // WebRTC answers a CONNECT Request by offering; for a pair
                    // that selects ICE+QUIC, offer ICE+QUIC instead.
                    if let Some(quic) = self.quic() {
                        let from = &data.sender_id;
                        if data.data != super::webrtc::RESTART_REQUEST_MARKER
                            && self.links.kind_for(from) == LinkKind::IceQuic
                        {
                            if quic.state(from).is_none() {
                                let _ = quic.connect(from).await;
                            }
                            return Ok(());
                        }
                    }
                }
                SignalingType::IceQuicOffer | SignalingType::IceQuicAnswer => {
                    match self.quic() {
                        Some(quic) if ICE_QUIC_LINK_WIRED => {
                            if data.signaling_type == SignalingType::IceQuicOffer {
                                quic.handle_offer(&data.sender_id, &data.data);
                            } else {
                                quic.handle_answer(&data.sender_id, &data.data);
                            }
                        }
                        _ => tracing::debug!(
                            "[LinkSelector] action=ignore type={:?} peer={} reason=ice_quic_disabled",
                            data.signaling_type,
                            data.sender_id.0
                        ),
                    }
                    return Ok(());
                }
                SignalingType::Offer => {
                    // The peer negotiates WebRTC: it fell back or does not
                    // select ICE+QUIC for us. Do not keep a competing attempt.
                    if let Some(quic) = self.quic() {
                        quic.abandon_pending(&data.sender_id);
                    }
                }
                _ => {}
            }
        }
        <WebRtcTransport as SignalingHandler>::handle_message(&self.links.webrtc, msg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mistlib_core::signaling::Signaler;

    #[test]
    fn local_caps_include_webrtc() {
        assert!(LOCAL_CAPS.supports(LinkKind::WebRtc));
        assert!(!LinkCaps::UNANNOUNCED.supports(LinkKind::WebRtc));
        assert_eq!(LinkCaps::default(), LinkCaps::UNANNOUNCED);
    }

    #[test]
    fn unannounced_or_webrtc_only_peers_select_webrtc() {
        for remote in [LinkCaps::UNANNOUNCED, LinkCaps::of(&[LinkKind::WebRtc])] {
            assert_eq!(select_link(LOCAL_CAPS, remote), LinkKind::WebRtc);
        }
    }

    #[test]
    fn selection_is_symmetric() {
        let all = [LinkCaps::UNANNOUNCED, LOCAL_CAPS];
        for a in all {
            for b in all {
                assert_eq!(select_link(a, b), select_link(b, a));
            }
        }
    }

    #[test]
    fn every_peer_resolves_to_the_webrtc_link() {
        let signaler: Arc<dyn Signaler> = Arc::new(NoopSignaler);
        let webrtc = Arc::new(WebRtcTransport::new(signaler, NodeId("local".into())));
        let selector = LinkSelector::new(webrtc.clone());
        for node in ["a", "b", ""] {
            let node = NodeId(node.into());
            assert_eq!(selector.links.kind_for(&node), LinkKind::WebRtc);
            let Link::WebRtc(link) = selector.link_for(&node) else {
                panic!("expected the WebRTC link");
            };
            assert!(Arc::ptr_eq(link, &webrtc));
        }
        assert!(Arc::ptr_eq(selector.webrtc(), &webrtc));
    }

    #[test]
    fn ice_quic_is_chosen_only_when_both_sides_announce_it() {
        let quic = LOCAL_CAPS_ICE_QUIC;
        let cases = [
            (LOCAL_CAPS, LinkCaps::UNANNOUNCED, LinkKind::WebRtc),
            (LOCAL_CAPS, quic, LinkKind::WebRtc),
            (quic, LinkCaps::UNANNOUNCED, LinkKind::WebRtc),
            (quic, LOCAL_CAPS, LinkKind::WebRtc),
            (quic, quic, LinkKind::IceQuic),
        ];
        for (local, remote, expected) in cases {
            assert_eq!(select_link(local, remote), expected, "{local:?} {remote:?}");
            assert_eq!(select_link(remote, local), expected, "symmetry");
        }
    }

    #[test]
    fn caps_from_announced_names_ignore_unknown_ones() {
        assert_eq!(LinkCaps::from_names([]), LinkCaps::UNANNOUNCED);
        assert_eq!(LinkCaps::from_names(["webrtc"]), LOCAL_CAPS);
        assert_eq!(
            LinkCaps::from_names(["webrtc", "iq1", "future-x"]),
            LOCAL_CAPS_ICE_QUIC
        );
        assert!(LinkCaps::from_names(["iq1"]).supports(LinkKind::IceQuic));
    }

    #[test]
    fn announcements_are_empty_unless_opted_in() {
        assert!(transports_for(false).is_empty());
        assert_eq!(encode_connect_request_data(transports_for(false)), "");
        let data = encode_connect_request_data(transports_for(true));
        let names = decode_connect_request_transports(&data).unwrap();
        assert_eq!(
            LinkCaps::from_names(names.iter().map(String::as_str)),
            LOCAL_CAPS_ICE_QUIC
        );
    }

    #[test]
    fn learned_caps_select_ice_quic_but_without_quic_peers_routing_stays_webrtc() {
        let signaler: Arc<dyn Signaler> = Arc::new(NoopSignaler);
        let webrtc = Arc::new(WebRtcTransport::new(signaler, NodeId("local".into())));
        let table = LinkTable::new(webrtc.clone(), LOCAL_CAPS_ICE_QUIC);
        let peer = NodeId("peer".into());
        assert_eq!(table.selected_kind(&peer), LinkKind::WebRtc);
        table.learn_remote_caps(&peer, LOCAL_CAPS_ICE_QUIC);
        // A later Request that announces nothing keeps what was learned.
        table.learn_remote_caps(&peer, LinkCaps::UNANNOUNCED);
        assert_eq!(table.selected_kind(&peer), LinkKind::IceQuic);
        // No IceQuicPeers attached (`webrtc.iceQuic: false` at session build): WebRTC.
        assert_eq!(table.kind_for(&peer), LinkKind::WebRtc);
        let Link::WebRtc(link) = table.link_for(&peer) else {
            panic!("expected the WebRTC link");
        };
        assert!(Arc::ptr_eq(link, &webrtc));
    }

    #[test]
    fn without_the_opt_in_no_caps_are_kept() {
        let signaler: Arc<dyn Signaler> = Arc::new(NoopSignaler);
        let webrtc = Arc::new(WebRtcTransport::new(signaler, NodeId("local".into())));
        let table = LinkTable::new(webrtc, LOCAL_CAPS);
        let peer = NodeId("peer".into());
        table.learn_remote_caps(&peer, LOCAL_CAPS_ICE_QUIC);
        assert!(table.remote.read().unwrap().is_empty());
        assert_eq!(table.selected_kind(&peer), LinkKind::WebRtc);
    }

    /// In-memory signaling between two selectors: whatever one sends is
    /// handled by the other (JSON round trip, like Nostr), unless `drop_quic`
    /// discards ICE+QUIC offers/answers to simulate a silent peer.
    struct PairSignaler {
        peer: std::sync::OnceLock<std::sync::Weak<LinkSelector>>,
        drop_quic: bool,
        /// Drop only the first ICE+QUIC offer (a lost signaling message).
        drop_first_offer: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl Signaler for PairSignaler {
        async fn send_signaling(&self, _to: &NodeId, content: MessageContent) -> Result<()> {
            if let MessageContent::Data(data) = &content {
                if self.drop_quic
                    && matches!(
                        data.signaling_type,
                        SignalingType::IceQuicOffer | SignalingType::IceQuicAnswer
                    )
                {
                    return Ok(());
                }
                if data.signaling_type == SignalingType::IceQuicOffer
                    && self
                        .drop_first_offer
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    return Ok(());
                }
                let json = serde_json::to_string(data).unwrap();
                let data: mistlib_core::signaling::SignalingData =
                    serde_json::from_str(&json).unwrap();
                if let Some(peer) = self.peer.get().and_then(|w| w.upgrade()) {
                    tokio::spawn(async move {
                        let _ = peer.handle_message(MessageContent::Data(data)).await;
                    });
                }
            }
            Ok(())
        }

        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    struct Collect(mpsc::UnboundedSender<mistlib_core::transport::NetworkEvent>);
    impl NetworkEventHandler for Collect {
        fn on_event(&self, event: mistlib_core::transport::NetworkEvent) {
            let _ = self.0.send(event);
        }
    }

    fn quic_selector(
        id: &str,
        signaler: Arc<PairSignaler>,
    ) -> (Arc<LinkSelector>, Arc<WebRtcTransport>) {
        let webrtc = Arc::new(WebRtcTransport::new(
            signaler.clone() as Arc<dyn Signaler>,
            NodeId(id.into()),
        ));
        let quic = IceQuicPeers::new(
            NodeId(id.into()),
            "room".into(),
            signaler as Arc<dyn Signaler>,
            crate::transports::ice_quic::IceQuicConfig {
                loopback_only: true,
                connect_timeout: std::time::Duration::from_secs(20),
                ..Default::default()
            },
            &webrtc,
        );
        let selector = Arc::new(
            LinkSelector::with_local_caps(webrtc.clone(), LOCAL_CAPS_ICE_QUIC).with_ice_quic(quic),
        );
        (selector, webrtc)
    }

    fn capability_request(from: &str, to: &str) -> MessageContent {
        MessageContent::Data(mistlib_core::signaling::SignalingData {
            sender_id: NodeId(from.into()),
            receiver_id: NodeId(to.into()),
            room_id: "room".into(),
            data: encode_connect_request_data(transports_for(true)),
            signaling_type: SignalingType::Request,
        })
    }

    async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !ready() {
            assert!(std::time::Instant::now() < deadline, "{what} timed out");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn announced_pair_connects_over_ice_quic_and_exchanges_messages() {
        let sig_a = Arc::new(PairSignaler {
            peer: Default::default(),
            drop_quic: false,
            drop_first_offer: Default::default(),
        });
        let sig_b = Arc::new(PairSignaler {
            peer: Default::default(),
            drop_quic: false,
            drop_first_offer: Default::default(),
        });
        let (a, _) = quic_selector("a", sig_a.clone());
        let (b, _) = quic_selector("b", sig_b.clone());
        sig_a.peer.set(Arc::downgrade(&b)).ok();
        sig_b.peer.set(Arc::downgrade(&a)).ok();
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        a.start(Arc::new(Collect(tx_a))).await.unwrap();
        b.start(Arc::new(Collect(tx_b))).await.unwrap();
        let (na, nb) = (NodeId("a".into()), NodeId("b".into()));

        // b (higher id) announces iq1 to a; a (lower id) then offers ICE+QUIC.
        b.handle_message(capability_request("a", "b"))
            .await
            .unwrap();
        a.handle_message(capability_request("b", "a"))
            .await
            .unwrap();
        wait_for("ICE+QUIC connected on both sides", || {
            a.get_connection_state(&nb) == ConnectionState::Connected
                && b.get_connection_state(&na) == ConnectionState::Connected
        })
        .await;
        assert!(matches!(a.link_for(&nb), Link::IceQuic(_)));
        assert_eq!(a.get_connected_nodes(), vec![nb.clone()]);
        // Stats report the pair under the ICE+QUIC link.
        assert_eq!(
            a.ice_quic_states(),
            vec![(nb.clone(), ConnectionState::Connected)]
        );

        for method in [
            DeliveryMethod::ReliableOrdered,
            DeliveryMethod::ReliableUnordered,
            DeliveryMethod::Unreliable,
        ] {
            a.send(&nb, Bytes::from_static(b"to-b"), method)
                .await
                .unwrap();
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), rx_b.recv())
                .await
                .expect("b received")
                .unwrap();
            assert_eq!(event.from, na);
            assert_eq!(&event.data[..], b"to-b");
        }
        // Large unreliable payloads move to a reliable stream instead of failing.
        let big = Bytes::from(vec![7u8; 8 * 1024]);
        b.send(&na, big.clone(), DeliveryMethod::Unreliable)
            .await
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), rx_a.recv())
            .await
            .expect("a received")
            .unwrap();
        assert_eq!(event.data, big);

        a.disconnect(&nb).await.unwrap();
        wait_for("b observes the close", || {
            b.get_connection_state(&na) != ConnectionState::Connected
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rtp_reaches_ice_quic_peers_as_media_packet_events() {
        use webrtc::api::media_engine::MIME_TYPE_OPUS;
        use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
        use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

        let sig_a = Arc::new(PairSignaler {
            peer: Default::default(),
            drop_quic: false,
            drop_first_offer: Default::default(),
        });
        let sig_b = Arc::new(PairSignaler {
            peer: Default::default(),
            drop_quic: false,
            drop_first_offer: Default::default(),
        });
        let (a, _) = quic_selector("a", sig_a.clone());
        let (b, _) = quic_selector("b", sig_b.clone());
        sig_a.peer.set(Arc::downgrade(&b)).ok();
        sig_b.peer.set(Arc::downgrade(&a)).ok();
        let (tx_a, _rx_a) = mpsc::unbounded_channel();
        let (tx_b, _rx_b) = mpsc::unbounded_channel();
        a.start(Arc::new(Collect(tx_a))).await.unwrap();
        b.start(Arc::new(Collect(tx_b))).await.unwrap();
        let (na, nb) = (NodeId("a".into()), NodeId("b".into()));
        let (media_tx, mut media_rx) = mpsc::unbounded_channel();
        crate::app::register_media_packet_handler(media_tx);

        b.handle_message(capability_request("a", "b"))
            .await
            .unwrap();
        a.handle_message(capability_request("b", "a"))
            .await
            .unwrap();
        wait_for("ICE+QUIC connected", || {
            a.get_connection_state(&nb) == ConnectionState::Connected
                && b.get_connection_state(&na) == ConnectionState::Connected
        })
        .await;

        let track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                ..Default::default()
            },
            "voice".to_owned(),
            "stream-a".to_owned(),
        ));
        a.publish_local_track(track).await.unwrap();
        for seq in 0..3u16 {
            let packet = webrtc::rtp::packet::Packet {
                header: webrtc::rtp::header::Header {
                    version: 2,
                    payload_type: 111,
                    sequence_number: seq,
                    timestamp: 960 * seq as u32,
                    ssrc: 42,
                    ..Default::default()
                },
                payload: Bytes::from(vec![seq as u8; 80]),
            };
            a.write_local_rtp("voice", &packet).await.unwrap();
        }
        let mut seen = Vec::new();
        while seen.len() < 3 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), media_rx.recv())
                .await
                .expect("media packet received")
                .unwrap();
            assert_eq!(event.remote_id, na);
            assert_eq!(event.room_id, "room");
            assert_eq!(event.track_id, "voice");
            assert_eq!(event.stream_id, "stream-a");
            assert_eq!(event.kind, "audio");
            assert_eq!(event.mime_type, MIME_TYPE_OPUS);
            assert_eq!(event.clock_rate, 48_000);
            assert_eq!(event.packet.payload.len(), 80);
            seen.push(event.packet.header.sequence_number);
        }
        seen.sort();
        assert_eq!(seen, vec![0, 1, 2]);
        // An unpublished track is an error, as before.
        assert!(a
            .write_local_rtp("missing", &webrtc::rtp::packet::Packet::default())
            .await
            .is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lost_offer_is_resent_and_the_pair_still_uses_ice_quic() {
        let sig_a = Arc::new(PairSignaler {
            peer: Default::default(),
            drop_quic: false,
            drop_first_offer: std::sync::atomic::AtomicBool::new(true),
        });
        let sig_b = Arc::new(PairSignaler {
            peer: Default::default(),
            drop_quic: false,
            drop_first_offer: Default::default(),
        });
        let (a, _) = quic_selector("a", sig_a.clone());
        let (b, _) = quic_selector("b", sig_b.clone());
        sig_a.peer.set(Arc::downgrade(&b)).ok();
        sig_b.peer.set(Arc::downgrade(&a)).ok();
        let (tx_a, _rx_a) = mpsc::unbounded_channel();
        let (tx_b, _rx_b) = mpsc::unbounded_channel();
        a.start(Arc::new(Collect(tx_a))).await.unwrap();
        b.start(Arc::new(Collect(tx_b))).await.unwrap();
        let (na, nb) = (NodeId("a".into()), NodeId("b".into()));
        b.handle_message(capability_request("a", "b"))
            .await
            .unwrap();
        a.handle_message(capability_request("b", "a"))
            .await
            .unwrap();
        wait_for("ICE+QUIC connected after the offer resend", || {
            a.get_connection_state(&nb) == ConnectionState::Connected
                && b.get_connection_state(&na) == ConnectionState::Connected
        })
        .await;
        assert!(matches!(a.link_for(&nb), Link::IceQuic(_)));
        assert!(!a.quic().unwrap().pinned_to_webrtc(&nb));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn silent_peer_pins_the_pair_to_webrtc() {
        let sig_a = Arc::new(PairSignaler {
            peer: Default::default(),
            drop_quic: true,
            drop_first_offer: Default::default(),
        });
        let sig_b = Arc::new(PairSignaler {
            peer: Default::default(),
            drop_quic: true,
            drop_first_offer: Default::default(),
        });
        let (a, _) = quic_selector("a", sig_a.clone());
        let (b, _) = quic_selector("b", sig_b.clone());
        sig_a.peer.set(Arc::downgrade(&b)).ok();
        sig_b.peer.set(Arc::downgrade(&a)).ok();
        let nb = NodeId("b".into());
        a.handle_message(capability_request("b", "a"))
            .await
            .unwrap();
        assert_eq!(a.links.selected_kind(&nb), LinkKind::IceQuic);
        // The offer never arrives; after the answer timeout a falls back.
        wait_for("fallback", || a.quic().unwrap().pinned_to_webrtc(&nb)).await;
        assert_eq!(a.links.kind_for(&nb), LinkKind::WebRtc);
        assert!(a.quic().unwrap().state(&nb).is_none());
    }

    struct NoopSignaler;
    #[async_trait]
    impl Signaler for NoopSignaler {
        async fn send_signaling(&self, _to: &NodeId, _content: MessageContent) -> Result<()> {
            Ok(())
        }

        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }
}
