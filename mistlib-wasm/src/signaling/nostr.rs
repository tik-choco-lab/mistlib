use async_trait::async_trait;
use mistlib_core::config::NostrSignalingConfig;
use mistlib_core::signaling::nostr::{
    build_discovery_event_with_joined_at, build_message_event_with_sequence_and_joined_at,
    event_frame_json, next_outgoing_sequence as next_nostr_sequence, random_subscription_id,
    DedupeCache, DiscoveryTable, InvitePskCrypto, NostrCodecConfig, TemporarySignalingIdentity,
};
use mistlib_core::signaling::{MessageContent, Signaler, SignalingData, SignalingType};
use mistlib_core::stats::STATS;
use mistlib_core::types::NodeId;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use web_sys::WebSocket;
use web_time::Duration;

mod connection;
mod handler;
mod keepalive;
mod refresh;
mod relay_source;
#[cfg(test)]
mod tests;

/// Interval between periodic REQ re-subscriptions on each active room.
///
/// Browser `WebSocket` has no ping API, so a JS-side idle proxy/NAT
/// disconnect (typically 30-120s) cannot be detected via WS pings the way
/// `mistlib-native` does. Instead, resending the (unchanged-id) REQ frame at
/// this cadence produces real outbound/inbound relay traffic that doubles as
/// keepalive, and also keeps the subscribed `discovery_filter`/`message_filter`
/// scope windows in sync with `NostrCodecConfig::room_scope_rotation_seconds`
/// (default 600s), which otherwise silently rotates out from under a
/// long-lived, never-refreshed subscription.
#[cfg(not(test))]
pub(super) const RELAY_KEEPALIVE_INTERVAL_MS: u32 = 30_000;
#[cfg(test)]
pub(super) const RELAY_KEEPALIVE_INTERVAL_MS: u32 = 200;

/// How long a relay connection may go without receiving any frame before it
/// is treated as dead and force-closed to trigger reconnection. Two
/// keepalive cycles, mirroring `mistlib-native`'s
/// `RELAY_PING_INTERVAL * 2` dead-connection threshold.
#[cfg(not(test))]
pub(super) const RELAY_IDLE_THRESHOLD_MS: u32 = 60_000;
#[cfg(test)]
pub(super) const RELAY_IDLE_THRESHOLD_MS: u32 = 400;

#[derive(Clone)]
pub struct WasmNostrSignaler {
    relays: Vec<String>,
    relay_list_url: Option<String>,
    local_node_id: NodeId,
    identity: TemporarySignalingIdentity,
    rotated_identity: Arc<Mutex<Option<TemporarySignalingIdentity>>>,
    codec_config: NostrCodecConfig,
    crypto: InvitePskCrypto,
    sockets: Arc<Mutex<Vec<WebSocket>>>,
    room_id: Arc<Mutex<Option<String>>>,
    discovery_table: Arc<Mutex<DiscoveryTable>>,
    dedupe: Arc<Mutex<DedupeCache>>,
    message_dedupe: Arc<Mutex<DedupeCache>>,
    outgoing_sequences: Arc<Mutex<HashMap<String, u64>>>,
    incoming_sequences: Arc<Mutex<HashMap<String, u64>>>,
    local_joined_at: Arc<Mutex<Option<u64>>>,
    requested_pubkeys: Arc<Mutex<HashSet<String>>>,
    peer_sessions: Arc<Mutex<HashMap<String, u64>>>,
    refresh_epoch: Arc<Mutex<u64>>,
    reconnect_epoch: Arc<Mutex<u64>>,
    keepalive_epoch: Arc<Mutex<u64>>,
    // Stable per-instance subscription ids for the discovery and message
    // REQ filters. Reused across the initial subscribe, every reconnect,
    // and every periodic keepalive re-subscription so that resending a REQ
    // replaces the relay's existing subscription (NIP-01) instead of piling
    // up an additional one each cycle.
    discovery_subscription_id: String,
    message_subscription_id: String,
}

impl WasmNostrSignaler {
    pub fn new(local_node_id: NodeId, config: NostrSignalingConfig) -> Self {
        let codec_config = NostrCodecConfig::from_config(&config);
        let crypto = InvitePskCrypto::new(&config.invite_salt, &config.invite_code);
        let dedupe_ttl = Duration::from_secs(config.ttl_seconds.saturating_mul(2).max(1));
        let relay_list_url = config.effective_relay_list_url().map(str::to_owned);
        Self {
            relays: config.relays,
            relay_list_url,
            local_node_id,
            identity: TemporarySignalingIdentity::generate(),
            rotated_identity: Arc::new(Mutex::new(None)),
            codec_config,
            crypto,
            sockets: Arc::new(Mutex::new(Vec::new())),
            room_id: Arc::new(Mutex::new(None)),
            discovery_table: Arc::new(Mutex::new(DiscoveryTable::default())),
            dedupe: Arc::new(Mutex::new(DedupeCache::new(dedupe_ttl))),
            message_dedupe: Arc::new(Mutex::new(DedupeCache::new(dedupe_ttl))),
            outgoing_sequences: Arc::new(Mutex::new(HashMap::new())),
            incoming_sequences: Arc::new(Mutex::new(HashMap::new())),
            local_joined_at: Arc::new(Mutex::new(None)),
            requested_pubkeys: Arc::new(Mutex::new(HashSet::new())),
            peer_sessions: Arc::new(Mutex::new(HashMap::new())),
            refresh_epoch: Arc::new(Mutex::new(0)),
            reconnect_epoch: Arc::new(Mutex::new(0)),
            keepalive_epoch: Arc::new(Mutex::new(0)),
            discovery_subscription_id: random_subscription_id(),
            message_subscription_id: random_subscription_id(),
        }
    }

    fn current_identity(&self) -> TemporarySignalingIdentity {
        self.rotated_identity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| self.identity.clone())
    }

    fn clear_session_state(&self) {
        self.discovery_table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.dedupe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.message_dedupe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.outgoing_sequences
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.incoming_sequences
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.requested_pubkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.peer_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Removes every piece of per-peer state keyed by `pubkey` from the
    /// tracking maps/sets that key on signaling pubkey rather than node id.
    ///
    /// Called from `handler.rs` when `DiscoveryTable::bind_node_with_epoch`
    /// reports a `Rebound` outcome (a node id just moved from `pubkey` to a
    /// new one, e.g. because the peer reloaded its page): `pubkey` is now
    /// dead and will never be seen again, so leaving its entries behind
    /// would just leak memory for the lifetime of the session. Does NOT
    /// touch `discovery_table` itself — `bind_node_with_epoch` already
    /// removed the dead pubkey's `by_pubkey` entry as part of the rebind.
    pub(super) fn purge_peer_state(&self, pubkey: &str) {
        self.requested_pubkeys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(pubkey);
        self.incoming_sequences
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(pubkey);
        self.outgoing_sequences
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(pubkey);
        self.peer_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(pubkey);
    }

    fn local_joined_at(&self) -> Option<u64> {
        *self
            .local_joined_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn mark_local_joined(&self) {
        *self
            .local_joined_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(current_unix_millis());
    }

    fn publish_frame(&self, frame: String) -> mistlib_core::error::Result<()> {
        let sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        if sockets.is_empty() {
            return Err(mistlib_core::error::MistError::Signaling(
                "WasmNostrSignaler: no relay connection is open".to_string(),
            ));
        }
        let mut sent = false;
        for ws in sockets.iter() {
            if ws.ready_state() == WebSocket::OPEN {
                ws.send_with_str(&frame).map_err(|e| {
                    mistlib_core::error::MistError::Network(format!(
                        "WasmNostrSignaler: relay send failed: {:?}",
                        e
                    ))
                })?;
                sent = true;
            }
        }
        if !sent {
            return Err(mistlib_core::error::MistError::Signaling(
                "WasmNostrSignaler: relay websocket is not open".to_string(),
            ));
        }
        STATS.add_send(frame.len() as u64);
        Ok(())
    }

    fn publish_event(
        &self,
        event: &mistlib_core::signaling::nostr::NostrEvent,
    ) -> mistlib_core::error::Result<()> {
        self.publish_frame(event_frame_json(event)?)
    }

    fn publish_discovery(&self, room_id: &str) -> mistlib_core::error::Result<()> {
        let identity = self.current_identity();
        let event = build_discovery_event_with_joined_at(
            &self.codec_config,
            &self.crypto,
            &identity,
            room_id,
            self.local_joined_at(),
        )?;
        self.publish_event(&event)
    }

    fn publish_message_to_pubkey(
        &self,
        receiver_pubkey: &str,
        data: &SignalingData,
    ) -> mistlib_core::error::Result<()> {
        // Locally-synthesized signals (currently only `Rejoin`) must never
        // reach a relay: they exist purely to notify our own transport layer
        // of a detected peer rebind. This is a defense-in-depth check —
        // `Signaler::send_signaling` already filters these out — kept here
        // too since this is the single choke point every publish path
        // (including the internal `Request` re-announce path) funnels
        // through.
        if data.signaling_type.is_local_only() {
            return Ok(());
        }
        let sequence = self.next_outgoing_sequence(receiver_pubkey);
        let identity = self.current_identity();
        let event = build_message_event_with_sequence_and_joined_at(
            &self.codec_config,
            &self.crypto,
            &identity,
            receiver_pubkey,
            data,
            sequence,
            self.local_joined_at(),
        )?;
        self.publish_event(&event)
    }

    fn next_outgoing_sequence(&self, receiver_pubkey: &str) -> u64 {
        let mut sequences = self
            .outgoing_sequences
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        next_nostr_sequence(&mut sequences, receiver_pubkey)
    }

    fn send_request_to_pubkey(
        &self,
        receiver_pubkey: &str,
        room_id: &str,
    ) -> mistlib_core::error::Result<()> {
        let request = SignalingData {
            sender_id: self.local_node_id.clone(),
            receiver_id: NodeId::broadcast(),
            room_id: room_id.to_string(),
            data: String::new(),
            signaling_type: SignalingType::Request,
        };
        self.publish_message_to_pubkey(receiver_pubkey, &request)
    }

    fn set_room_id(&self, room_id: &str) -> mistlib_core::error::Result<()> {
        if room_id.is_empty() {
            return Ok(());
        }
        let changed = {
            let mut room = self.room_id.lock().unwrap_or_else(|e| e.into_inner());
            if room.as_deref() == Some(room_id) {
                false
            } else {
                *room = Some(room_id.to_string());
                true
            }
        };
        if changed {
            self.discovery_table
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.dedupe
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.message_dedupe
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.outgoing_sequences
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.incoming_sequences
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.requested_pubkeys
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.peer_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.mark_local_joined();
            self.subscribe_room(room_id)?;
            self.spawn_discovery_refresh(room_id.to_string());
            self.spawn_relay_keepalive(room_id.to_string());
        }
        Ok(())
    }

    fn subscribe_room(&self, room_id: &str) -> mistlib_core::error::Result<()> {
        let sockets = self
            .sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for ws in sockets {
            if ws.ready_state() == WebSocket::OPEN {
                self.send_subscriptions(&ws, room_id)?;
            }
        }
        Ok(())
    }

    pub(super) fn next_reconnect_epoch(&self) -> u64 {
        let mut epoch = self
            .reconnect_epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *epoch = epoch.saturating_add(1);
        *epoch
    }

    pub(super) fn reconnect_epoch_matches(&self, expected_epoch: u64) -> bool {
        *self
            .reconnect_epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            == expected_epoch
    }

    pub(super) fn room_id(&self) -> Option<String> {
        self.room_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(super) fn remove_socket(&self, ws: &WebSocket) {
        self.sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|current| !js_sys::Object::is(current.as_ref(), ws.as_ref()));
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl Signaler for WasmNostrSignaler {
    async fn send_signaling(
        &self,
        to: &NodeId,
        msg: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        let MessageContent::Data(data) = msg else {
            return Err(mistlib_core::error::MistError::Signaling(
                "WasmNostrSignaler: unsupported message type".to_string(),
            ));
        };

        // Locally-synthesized signals (e.g. `Rejoin`) are never published to
        // a relay; they only ever travel over the local `incoming_tx`
        // channel from `handler.rs`. This path should be unreachable today
        // (nothing constructs a `Rejoin` and hands it to `send_signaling`),
        // but guard it anyway so a future caller can't leak one onto the
        // wire.
        if data.signaling_type.is_local_only() {
            return Ok(());
        }

        self.set_room_id(&data.room_id)?;

        if to.is_server() || to.is_broadcast() {
            if !data.room_id.is_empty() {
                self.publish_discovery(&data.room_id)?;
            }
            return Ok(());
        }

        let pubkey = self
            .discovery_table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pubkey_for_node(to)
            .ok_or_else(|| mistlib_core::error::MistError::RouteNotFound(to.clone()))?;

        self.publish_message_to_pubkey(&pubkey, &data)
    }

    async fn reset_session(&self) -> mistlib_core::error::Result<()> {
        let Some(room_id) = self
            .room_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return Ok(());
        };
        self.cancel_discovery_refresh();
        *self
            .rotated_identity
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(TemporarySignalingIdentity::generate());
        self.clear_session_state();
        self.mark_local_joined();
        self.spawn_discovery_refresh(room_id.clone());
        self.publish_discovery(&room_id)
    }

    async fn close(&self) -> mistlib_core::error::Result<()> {
        self.cancel_discovery_refresh();
        self.cancel_relay_keepalive();
        self.next_reconnect_epoch();
        let mut sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        for ws in sockets.drain(..) {
            let _ = ws.close();
        }
        self.room_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.clear_session_state();
        self.local_joined_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.rotated_identity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        Ok(())
    }
}

fn current_unix_millis() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
