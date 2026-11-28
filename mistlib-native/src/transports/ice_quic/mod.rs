//! ICE+QUIC link for native↔native peers (SPEC-25 §1 / §6.3, SPEC-24).
//!
//! A standalone point-to-point link: webrtc-ice finds and nominates a
//! candidate pair, and quinn runs QUIC over the nominated ICE connection.
//! SPEC-24 / `docs/investigation/SPEC25_O1_KR34_2026-11-27.md` measured it
//! against WebRTC DataChannels (ICE + DTLS + SCTP): under loss it wins even
//! with the same congestion controller. This module is the production
//! version of the research implementation in `tests/quicprobe`.
//!
//! **Not wired yet.** Nothing outside the tests constructs it. The next step
//! (SPEC-25 O2) adds a `LinkKind::IceQuic` to [`super::selector`]; see
//! "Planned wiring" at the end.
//!
//! # Setup
//!
//! ```text
//! offerer (ICE controlling, QUIC client)      answerer (ICE controlled, QUIC server)
//! IceQuicOfferer::gather(cfg)
//!   offer() ── IceQuicOffer ──(signaling)──▶  IceQuicAnswerer::gather(cfg, offer)
//!           ◀──(signaling)── IceQuicAnswer ──  answer()
//! connect(answer)                             connect()
//!   ICE dial → QUIC connect ─────────────────▶ ICE accept → QUIC accept
//!   open ordered stream ─────────────────────▶ accept ordered stream
//! IceQuicLink                                 IceQuicLink
//! ```
//!
//! - **Signaling** ([`IceQuicParams`], protocol version `"iq1"`): ICE
//!   ufrag/pwd, the full candidate list, the SHA-256 of the sender's
//!   certificate and its receive limit for reliable messages. Candidates are
//!   gathered completely before the payload is produced (non-trickle, as in
//!   quicprobe). Trickle ICE is future work: it needs a candidate message type
//!   and buffering like the WebRTC transport's pending-candidate table.
//! - **Roles**: the offerer is ICE-controlling and the QUIC client; the
//!   answerer is ICE-controlled and the QUIC server. The offer/answer
//!   direction alone decides both, so there is no tie-break.
//! - **Authentication**: each side generates a fresh self-signed certificate
//!   per link. TLS 1.3 runs with mutual authentication; each side accepts
//!   only the exact leaf whose SHA-256 the peer signaled ([`pin`]). The DNS
//!   name and CA are ignored, CertificateVerify is still checked (the peer
//!   must hold the key). This is the DTLS-fingerprint model of WebRTC: it is
//!   as trustworthy as the signaling channel. ALPN is `mistlib-iq1`.
//! - **ICE timers**: ICE STUN keepalives are disabled once connected
//!   (`keepalive_interval = 0`); QUIC PINGs keep the NAT binding and the ICE
//!   pair's last-received time fresh. ICE disconnected/failed timeouts are
//!   set to the QUIC idle timeout, so QUIC idle detection fires first and ICE
//!   failure is only a backstop.
//!
//! # Data path ([`IceQuicLink::send`])
//!
//! - `ReliableOrdered`: one long-lived bidirectional stream, opened by the
//!   client with a tag byte; each message is a frame with a `u32` BE length
//!   prefix. Each direction is its own ordered byte stream.
//! - `ReliableUnordered`: one unidirectional stream per message (tag byte +
//!   payload, FIN ends it). Streams are read concurrently, so a lost packet
//!   delays only its own message.
//! - `Unreliable`: one QUIC DATAGRAM, 1-byte header.
//! - `UnreliableOrdered`: one QUIC DATAGRAM, 1-byte header + `u16`
//!   sequence. The receiver drops datagrams not newer than the newest one
//!   delivered (RFC 1982 serial comparison), the analogue of the WebRTC
//!   `ordered, maxRetransmits: 0` channel.
//!
//! Datagrams are never fragmented or promoted to a stream: a payload larger
//! than [`IceQuicLink::max_datagram_payload`], or a peer without DATAGRAM
//! support, is an error returned to the caller ([`IceQuicError::TooLarge`],
//! [`IceQuicError::DatagramUnsupported`]). Datagram sends do not wait: when
//! quinn's send buffer is full it drops the oldest queued datagram, which is
//! the right failure for unreliable state updates. The reliable limit is
//! `min(local, remote)` of the configured `max_message_bytes` (both are
//! signaled), so a sender never produces a frame its peer rejects; a frame
//! above the receiver's own limit is a protocol violation and closes the
//! connection.
//!
//! # Failure detection vs. idle traffic
//!
//! Defaults: QUIC keep-alive 2.5 s, idle timeout 7.5 s ([`IceQuicConfig`]).
//! A silent peer is reported (`Failed`) after the idle timeout plus up to one
//! PTO, i.e. ≈ 8 s. WebRTC in mistlib reports `Disconnected` after ≈ 4 s
//! (ICE keepalive 2 s, disconnected 5 s; measured 4.12–4.13 s). SPEC-24 M5
//! measured the trade-off: QUIC with PING 2 s / idle 5 s detected in
//! ≈ 6.0 s at 140 B/s/connection idle, vs. WebRTC's 129 B/s at 4.1 s, so
//! matching WebRTC's detection time costs more idle traffic than WebRTC.
//! These defaults choose a slower detection (keep-alive ≥ 1/3 of the idle
//! timeout, so two PINGs may be lost) for fewer idle packets than the
//! measured `idle-eq` setting. Both values are configurable; the effective
//! idle timeout is the minimum of the two peers' values.
//!
//! # Congestion control
//!
//! [`IceQuicCongestion::Bbr3`] (default): the BBRv3 controller in [`bbr3`],
//! ported from `tests/quicprobe/src/bbr3`. It needs the explicit pacing rate
//! and per-packet callbacks of the patched quinn-proto in
//! `third_party/quinn-proto`. quinn's CUBIC and BBRv1 remain selectable.
//!
//! # Planned wiring (SPEC-25 O2, not done here)
//!
//! - Capabilities: add `LinkKind::IceQuic` to `LOCAL_CAPS`, announce it in
//!   the capability exchange, select it in `select_link` only when both sides
//!   announce it (browsers never do).
//! - Signaling: carry [`IceQuicOffer`] / [`IceQuicAnswer`] as new
//!   `MessageContent` types over the existing signaler (Nostr). The offerer
//!   must be decided symmetrically (e.g. the WebRTC initial-offerer rule).
//! - Fallback: a gather/connect error or timeout pins the pair to WebRTC
//!   (the selector's planned per-peer fallback row).
//! - Connection limit / handshake concurrency: count ICE+QUIC links in the
//!   same peer budget as WebRTC peers.
//! - Events: map [`IceQuicState`] to `ConnectionState` / network events, and
//!   [`IceQuicLink::message_size_limit`] to `Transport::message_size_limit`
//!   (the datagram limit is per method, smaller than the reliable one).

mod bbr3;
mod connect;
mod link;
pub(crate) mod peers;
mod pin;
mod signal;
mod socket;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use mistlib_core::types::DeliveryMethod;
use webrtc::ice::url::Url;
use webrtc::ice_transport::ice_server::RTCIceServer;

pub(crate) use connect::{IceQuicAnswerer, IceQuicOfferer};
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use link::{IceQuicLink, IceQuicStats};
pub(crate) use peers::IceQuicPeers;
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use signal::{IceQuicAnswer, IceQuicOffer, IceQuicParams, PROTOCOL_VERSION};

/// ALPN of the ICE+QUIC link. Changing the framing or the stream layout
/// needs a new value (and a new [`PROTOCOL_VERSION`]).
pub(crate) const ALPN: &[u8] = b"mistlib-iq1";

/// Default QUIC keep-alive (PING) interval.
pub(crate) const DEFAULT_KEEP_ALIVE: Duration = Duration::from_millis(2_500);
/// Default QUIC idle timeout. See the module docs for the detection time.
pub(crate) const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_millis(7_500);
/// See [`IceQuicConfig::quic_silence_timeout`]. With [`INITIAL_RTT`] the
/// client sends its Initial three times within this window.
pub(crate) const DEFAULT_QUIC_SILENCE_TIMEOUT: Duration = Duration::from_secs(2);
/// RTT estimate before the first sample (see `transport_config`).
const INITIAL_RTT: Duration = Duration::from_millis(100);
/// Default limit for one reliable message; matches the default of
/// `Config::limits.max_message_bytes`.
pub(crate) const DEFAULT_MAX_MESSAGE_BYTES: u32 = 65_536;

/// Congestion controller for the QUIC connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum IceQuicCongestion {
    /// BBRv3 (draft-ietf-ccwg-bbr-06, [`bbr3`]), on the patched quinn-proto
    /// in `third_party/quinn-proto`. The default: it was the best controller
    /// in the SPEC-24/25 measurements.
    #[default]
    Bbr3,
    /// quinn's CUBIC (selectable for comparisons; not used by default).
    #[allow(dead_code)]
    Cubic,
    /// quinn's built-in BBR (BBRv1).
    #[cfg_attr(not(test), allow(dead_code))]
    Bbr,
}

/// Configuration of one ICE+QUIC link. Both sides should use compatible
/// values; the idle timeout and the reliable message limit are negotiated
/// down to the smaller one.
#[derive(Clone, Debug)]
pub(crate) struct IceQuicConfig {
    /// STUN/TURN servers, in the shape the WebRTC transport uses
    /// (`map_ice_servers(&Config::webrtc.ice_servers)`). Empty = host
    /// candidates only.
    pub ice_servers: Vec<RTCIceServer>,
    pub congestion: IceQuicCongestion,
    /// QUIC PING interval. `None` disables keep-alive (the link then fails
    /// after `idle_timeout` of application silence).
    pub keep_alive: Option<Duration>,
    pub idle_timeout: Duration,
    /// Largest reliable message this side sends or accepts. Signaled; the
    /// effective limit is the minimum of both sides.
    pub max_message_bytes: u32,
    /// Upper bound for candidate gathering. Gathering continues in the
    /// background no longer than this; whatever was gathered is signaled.
    pub gather_timeout: Duration,
    /// Upper bound from applying the remote payload to an established link
    /// (ICE checks + QUIC handshake + ordered stream).
    pub connect_timeout: Duration,
    /// After ICE connects, give up when not a single QUIC datagram has
    /// arrived from the peer for this long: the path carries STUN but drops
    /// QUIC (a middlebox that blocks QUIC), so falling back to WebRTC now
    /// beats waiting for the handshake to time out (SPEC-25 KR 2.4). A lossy
    /// but open path almost always delivers one of the retransmitted
    /// Initials within this window; a slow handshake that has heard from the
    /// peer is left to `connect_timeout`.
    pub quic_silence_timeout: Duration,
    /// Tests only: discard every outgoing QUIC datagram.
    pub drop_outgoing_quic: bool,
    /// Gather only IPv4 loopback host candidates. For in-process tests and
    /// lab setups; never for production.
    pub loopback_only: bool,
}

impl Default for IceQuicConfig {
    fn default() -> Self {
        Self {
            ice_servers: Vec::new(),
            congestion: IceQuicCongestion::default(),
            keep_alive: Some(DEFAULT_KEEP_ALIVE),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            gather_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
            quic_silence_timeout: DEFAULT_QUIC_SILENCE_TIMEOUT,
            drop_outgoing_quic: false,
            loopback_only: false,
        }
    }
}

impl IceQuicConfig {
    /// A configuration using the ICE servers configured for WebRTC
    /// (`Config::webrtc.ice_servers`), with the same filtering of unusable
    /// entries.
    pub(crate) fn with_ice_servers(configured: &[mistlib_core::config::IceServer]) -> Self {
        Self {
            ice_servers: super::webrtc::map_ice_servers(configured),
            ..Self::default()
        }
    }

    /// The webrtc-ice URLs for [`Self::ice_servers`]. An unparseable URL is
    /// skipped with a warning (the WebRTC transport would reject the whole
    /// PeerConnection instead; here the link can still use host candidates).
    pub(crate) fn ice_urls(&self) -> Vec<Url> {
        let mut urls = Vec::new();
        for server in &self.ice_servers {
            for raw in &server.urls {
                match Url::parse_url(raw) {
                    Ok(mut url) => {
                        url.username.clone_from(&server.username);
                        url.password.clone_from(&server.credential);
                        urls.push(url);
                    }
                    Err(error) => {
                        tracing::warn!(url = %raw, %error, "ICE+QUIC: ignoring ICE server URL");
                    }
                }
            }
        }
        urls
    }
}

/// Connection state of a link, observed through
/// [`IceQuicLink::state`] / [`IceQuicOfferer::state`] (a `watch` channel).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IceQuicState {
    /// Gathering, waiting for the remote payload, ICE checks or the QUIC
    /// handshake.
    Connecting,
    Connected,
    /// Closed normally, by either side.
    Disconnected,
    /// Setup failed, the peer went silent (idle timeout), ICE failed, or a
    /// protocol/transport error closed the connection.
    Failed,
}

/// Errors of setup and of the send path.
#[derive(Debug, thiserror::Error)]
pub(crate) enum IceQuicError {
    #[error("ICE: {0}")]
    Ice(#[from] webrtc::ice::Error),
    #[error("ICE connectivity checks failed")]
    IceFailed,
    #[error("invalid ICE+QUIC signaling payload: {0}")]
    Signal(String),
    #[error("TLS/QUIC setup: {0}")]
    Setup(String),
    #[error("remote certificate does not match the signaled fingerprint")]
    FingerprintMismatch,
    #[error("ICE+QUIC setup timed out after {0:?}")]
    Timeout(Duration),
    #[error("no QUIC datagram from the peer within {0:?} of ICE connecting (QUIC blocked?)")]
    QuicSilent(Duration),
    #[error("QUIC connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("{method:?} message of {size} bytes exceeds the {limit}-byte limit")]
    TooLarge {
        method: DeliveryMethod,
        size: usize,
        limit: usize,
    },
    #[error("peer does not support QUIC DATAGRAM")]
    DatagramUnsupported,
    #[error("link closed")]
    Closed,
}

impl From<quinn::WriteError> for IceQuicError {
    fn from(error: quinn::WriteError) -> Self {
        match error {
            quinn::WriteError::ConnectionLost(error) => Self::Connection(error),
            _ => Self::Closed,
        }
    }
}

/// Application error codes in CONNECTION_CLOSE.
pub(crate) mod close_code {
    /// Normal close; the peer reports `Disconnected`.
    pub(crate) const NORMAL: u32 = 0;
    /// The peer sent something that violates the framing.
    pub(crate) const PROTOCOL_VIOLATION: u32 = 1;
    /// ICE reported failure under a live QUIC connection.
    pub(crate) const ICE_FAILED: u32 = 2;
}

/// The transport config shared by both roles.
fn transport_config(config: &IceQuicConfig) -> Result<Arc<quinn::TransportConfig>, IceQuicError> {
    let idle_timeout = quinn::IdleTimeout::try_from(config.idle_timeout)
        .map_err(|_| IceQuicError::Setup("idle timeout out of range".into()))?;
    let mut transport = quinn::TransportConfig::default();
    transport
        .keep_alive_interval(config.keep_alive)
        .max_idle_timeout(Some(idle_timeout))
        // RFC 9002's 333 ms default puts the first Initial retransmit ~1 s
        // out; with 100 ms (Chromium's default) a lost Initial is resent at
        // ~0.3 s and ~0.9 s, three tries inside `quic_silence_timeout`, so a
        // lossy open path is not mistaken for one that blocks QUIC. Only the
        // pre-sample estimate changes; the first RTT sample replaces it.
        .initial_rtt(INITIAL_RTT)
        // Exactly one bidirectional stream (the ordered channel), opened by
        // the client.
        .max_concurrent_bidi_streams(1u32.into())
        .datagram_receive_buffer_size(Some(1024 * 1024))
        .datagram_send_buffer_size(1024 * 1024)
        // As measured in SPEC-24/25: 1200-byte packets, no PLPMTUD (the ICE
        // socket may fragment, so quinn would not probe anyway), no GSO.
        .initial_mtu(1200)
        .mtu_discovery_config(None)
        .enable_segmentation_offload(false);
    transport.congestion_controller_factory(match config.congestion {
        IceQuicCongestion::Bbr3 => Arc::new(bbr3::Factory) as Arc<_>,
        IceQuicCongestion::Cubic => Arc::new(quinn::congestion::CubicConfig::default()) as Arc<_>,
        IceQuicCongestion::Bbr => Arc::new(quinn::congestion::BbrConfig::default()) as Arc<_>,
    });
    Ok(Arc::new(transport))
}
