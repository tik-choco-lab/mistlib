//! Link setup: gather → exchange payloads → ICE → QUIC → ordered stream.

use std::net::IpAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::sync::{mpsc, oneshot, watch};
use util::Conn;
use webrtc::ice::agent::agent_config::{AgentConfig, IpFilterFn};
use webrtc::ice::agent::Agent;
use webrtc::ice::candidate::candidate_base::unmarshal_candidate;
use webrtc::ice::candidate::Candidate;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice::state::ConnectionState as IceConnectionState;

use super::link::{LinkParts, STREAM_ORDERED};
use super::pin::{fingerprint, Pin};
use super::signal::{IceQuicAnswer, IceQuicOffer, IceQuicParams, PROTOCOL_VERSION};
use super::socket::IceSocket;
use super::{transport_config, IceQuicConfig, IceQuicError, IceQuicLink, IceQuicState, ALPN};

/// TLS server name. Not verified (the certificate is pinned); `.invalid`
/// so it can never be mistaken for a real host.
const SERVER_NAME: &str = "mistlib.invalid";

/// Closes the ICE agent unless ownership moved on to a link. Covers setup
/// errors and a dropped (cancelled) setup future.
struct AgentGuard(Option<Arc<Agent>>);

impl AgentGuard {
    fn agent(&self) -> &Arc<Agent> {
        self.0.as_ref().expect("agent present until taken")
    }

    fn take(mut self) -> Arc<Agent> {
        self.0.take().expect("agent present until taken")
    }
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let Some(agent) = self.0.take() else { return };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = agent.close().await;
            });
        }
    }
}

/// Gathered local side, shared by both roles.
struct Gathered {
    agent: AgentGuard,
    cert: rcgen::CertifiedKey,
    local: IceQuicParams,
    config: IceQuicConfig,
    controlling: bool,
    state: watch::Sender<IceQuicState>,
    ice_state: watch::Receiver<IceConnectionState>,
}

/// The offering side: ICE-controlling, QUIC client.
pub(crate) struct IceQuicOfferer(Gathered);

impl IceQuicOfferer {
    /// Create the ICE agent and certificate and gather candidates.
    pub(crate) async fn gather(config: IceQuicConfig) -> Result<Self, IceQuicError> {
        Ok(Self(Gathered::new(config, true).await?))
    }

    /// The payload to signal to the answerer.
    pub(crate) fn offer(&self) -> IceQuicOffer {
        IceQuicOffer(self.0.local.clone())
    }

    /// Setup state (`Connecting` until [`Self::connect`] resolves).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn state(&self) -> watch::Receiver<IceQuicState> {
        self.0.state.subscribe()
    }

    /// Apply the answer and connect.
    pub(crate) async fn connect(self, answer: IceQuicAnswer) -> Result<IceQuicLink, IceQuicError> {
        self.0.connect(answer.0).await
    }
}

/// The answering side: ICE-controlled, QUIC server.
pub(crate) struct IceQuicAnswerer {
    gathered: Gathered,
    remote: IceQuicParams,
}

impl IceQuicAnswerer {
    /// Validate the offer, then create the ICE agent and certificate and
    /// gather candidates.
    pub(crate) async fn gather(
        config: IceQuicConfig,
        offer: IceQuicOffer,
    ) -> Result<Self, IceQuicError> {
        offer.0.validate()?;
        Ok(Self {
            gathered: Gathered::new(config, false).await?,
            remote: offer.0,
        })
    }

    /// The payload to signal back to the offerer.
    pub(crate) fn answer(&self) -> IceQuicAnswer {
        IceQuicAnswer(self.gathered.local.clone())
    }

    /// Setup state (`Connecting` until [`Self::connect`] resolves).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn state(&self) -> watch::Receiver<IceQuicState> {
        self.gathered.state.subscribe()
    }

    /// Connect to the offerer. Call after sending [`Self::answer`].
    pub(crate) async fn connect(self) -> Result<IceQuicLink, IceQuicError> {
        self.gathered.connect(self.remote).await
    }
}

/// webrtc-ice gathers nothing for an empty list (it does not default it),
/// so always name the networks explicitly.
fn network_types(loopback_only: bool) -> Vec<NetworkType> {
    if loopback_only {
        vec![NetworkType::Udp4]
    } else {
        webrtc::ice::network_type::supported_network_types()
    }
}

impl Gathered {
    async fn new(config: IceQuicConfig, controlling: bool) -> Result<Self, IceQuicError> {
        let loopback = config.loopback_only;
        let ip_filter: Option<IpFilterFn> = if loopback {
            Some(Box::new(|ip: IpAddr| ip.is_loopback()))
        } else {
            None
        };
        let agent = Arc::new(
            Agent::new(AgentConfig {
                urls: config.ice_urls(),
                network_types: network_types(loopback),
                // Native peers signal IP candidates; no mDNS socket needed.
                multicast_dns_mode: MulticastDnsMode::Disabled,
                is_controlling: controlling,
                // QUIC PINGs keep the binding and the pair's last-received
                // time fresh; QUIC idle timeout is the primary detector and
                // ICE failure only a backstop (module docs).
                keepalive_interval: Some(Duration::ZERO),
                disconnected_timeout: Some(config.idle_timeout),
                failed_timeout: Some(config.idle_timeout),
                include_loopback: loopback,
                ip_filter: Arc::new(ip_filter),
                ..Default::default()
            })
            .await?,
        );
        let (state, _) = watch::channel(IceQuicState::Connecting);
        let (ice_tx, ice_state) = watch::channel(IceConnectionState::New);
        agent.on_connection_state_change(Box::new(move |ice| {
            tracing::debug!(state = %ice, "ICE+QUIC: ICE state");
            ice_tx.send_replace(ice);
            Box::pin(async {})
        }));
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let done_tx = StdMutex::new(Some(done_tx));
        agent.on_candidate(Box::new(move |candidate| {
            // `None` marks the end of gathering.
            if candidate.is_none() {
                if let Some(done) = done_tx.lock().expect("gather mutex poisoned").take() {
                    let _ = done.send(());
                }
            }
            Box::pin(async {})
        }));
        let agent = AgentGuard(Some(agent));
        agent.agent().gather_candidates()?;
        if tokio::time::timeout(config.gather_timeout, done_rx)
            .await
            .is_err()
        {
            tracing::debug!(
                timeout = ?config.gather_timeout,
                "ICE+QUIC: gathering timed out; signaling the candidates gathered so far"
            );
        }

        let (ice_ufrag, ice_pwd) = agent.agent().get_local_user_credentials().await;
        let candidates: Vec<String> = agent
            .agent()
            .get_local_candidates()
            .await?
            .iter()
            .map(|candidate| candidate.marshal())
            .collect();
        if candidates.is_empty() {
            return Err(IceQuicError::Setup("no local ICE candidates".into()));
        }
        let cert = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])
            .map_err(|error| IceQuicError::Setup(format!("certificate: {error}")))?;
        let local = IceQuicParams {
            version: PROTOCOL_VERSION.to_string(),
            ice_ufrag,
            ice_pwd,
            candidates,
            cert_sha256: fingerprint(cert.cert.der()),
            max_message_bytes: config.max_message_bytes,
        };
        Ok(Self {
            agent,
            cert,
            local,
            config,
            controlling,
            state,
            ice_state,
        })
    }

    async fn connect(self, remote: IceQuicParams) -> Result<IceQuicLink, IceQuicError> {
        let state = self.state.clone();
        let timeout = self.config.connect_timeout;
        let result = match tokio::time::timeout(timeout, self.establish(remote)).await {
            Ok(result) => result,
            Err(_) => Err(IceQuicError::Timeout(timeout)),
        };
        if result.is_err() {
            state.send_replace(IceQuicState::Failed);
        }
        result
    }

    async fn establish(self, remote: IceQuicParams) -> Result<IceQuicLink, IceQuicError> {
        remote.validate()?;
        let Self {
            agent,
            cert,
            local,
            config,
            controlling,
            state,
            mut ice_state,
        } = self;

        let mut added = 0;
        for raw in &remote.candidates {
            match unmarshal_candidate(raw) {
                Ok(candidate) => {
                    let candidate: Arc<dyn Candidate + Send + Sync> = Arc::new(candidate);
                    match agent.agent().add_remote_candidate(&candidate) {
                        Ok(()) => added += 1,
                        Err(error) => tracing::debug!(%error, "ICE+QUIC: remote candidate"),
                    }
                }
                Err(error) => tracing::debug!(%error, "ICE+QUIC: unparseable remote candidate"),
            }
        }
        if added == 0 {
            return Err(IceQuicError::Signal("no usable remote candidate".into()));
        }

        // Keep the sender alive: dropping it cancels dial/accept.
        let (_cancel, cancel_rx) = mpsc::channel(1);
        let ufrag = remote.ice_ufrag.clone();
        let pwd = remote.ice_pwd.clone();
        let ice = async {
            let conn: Arc<dyn Conn + Send + Sync> = if controlling {
                agent.agent().dial(cancel_rx, ufrag, pwd).await?
            } else {
                agent.agent().accept(cancel_rx, ufrag, pwd).await?
            };
            Ok::<_, IceQuicError>(conn)
        };
        let ice_failed = async {
            loop {
                if *ice_state.borrow_and_update() == IceConnectionState::Failed {
                    return;
                }
                if ice_state.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        };
        let conn = tokio::select! {
            conn = ice => conn?,
            () = ice_failed => return Err(IceQuicError::IceFailed),
        };
        let peer = conn
            .remote_addr()
            .ok_or_else(|| IceQuicError::Setup("ICE nominated no remote address".into()))?;
        if let Some(pair) = agent.agent().get_selected_candidate_pair() {
            tracing::debug!(pair = %pair, "ICE+QUIC: ICE connected");
        }
        let socket = IceSocket::new(conn, config.drop_outgoing_quic)
            .map_err(|error| IceQuicError::Setup(format!("socket: {error}")))?;
        let heard = socket.heard_from_peer();
        let silence_timeout = config.quic_silence_timeout;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let pin = Pin::new(remote.cert_sha256.clone(), provider.clone());
        let chain = vec![cert.cert.der().clone()];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
        let transport = transport_config(&config)?;
        let runtime = Arc::new(quinn::TokioRuntime);
        let setup = |error: &dyn std::fmt::Display| IceQuicError::Setup(error.to_string());
        let handshake = |error: quinn::ConnectionError| {
            if pin.mismatched() {
                IceQuicError::FingerprintMismatch
            } else {
                IceQuicError::Connection(error)
            }
        };

        let quic = async {
            if controlling {
                let tls = client_tls(provider, pin.clone(), chain, key)?;
                let mut client = quinn::ClientConfig::new(Arc::new(
                    QuicClientConfig::try_from(tls).map_err(|e| setup(&e))?,
                ));
                client.transport_config(transport);
                let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
                    quinn::EndpointConfig::default(),
                    None,
                    socket,
                    runtime,
                )
                .map_err(|e| setup(&e))?;
                endpoint.set_default_client_config(client);
                let conn = endpoint
                    .connect(peer, SERVER_NAME)
                    .map_err(|e| setup(&e))?
                    .await
                    .map_err(handshake)?;
                // A QUIC stream exists for the peer only once it carries data:
                // the tag byte opens it.
                let (mut send, recv) = conn.open_bi().await?;
                send.write_all(&[STREAM_ORDERED]).await?;
                Ok::<_, IceQuicError>((endpoint, conn, send, recv))
            } else {
                let tls = server_tls(provider, pin.clone(), chain, key)?;
                let mut server = quinn::ServerConfig::with_crypto(Arc::new(
                    QuicServerConfig::try_from(tls).map_err(|e| setup(&e))?,
                ));
                server.transport_config(transport).migration(false);
                let endpoint = quinn::Endpoint::new_with_abstract_socket(
                    quinn::EndpointConfig::default(),
                    Some(server),
                    socket,
                    runtime,
                )
                .map_err(|e| setup(&e))?;
                let incoming = endpoint.accept().await.ok_or(IceQuicError::Closed)?;
                let conn = incoming.await.map_err(handshake)?;
                // One connection per link: refuse anything else.
                endpoint.set_server_config(None);
                let (send, mut recv) = conn.accept_bi().await?;
                let mut tag = [0];
                recv.read_exact(&mut tag)
                    .await
                    .map_err(|e| IceQuicError::Setup(format!("ordered stream: {e}")))?;
                if tag[0] != STREAM_ORDERED {
                    conn.close(super::close_code::PROTOCOL_VIOLATION.into(), b"stream tag");
                    return Err(IceQuicError::Setup("unexpected ordered stream tag".into()));
                }
                Ok((endpoint, conn, send, recv))
            }
        };
        // ICE works but the path may drop QUIC: give up early when the peer
        // has not sent a single QUIC datagram (see `quic_silence_timeout`).
        let silent = async {
            tokio::time::sleep(silence_timeout).await;
            if heard.load(std::sync::atomic::Ordering::Relaxed) {
                std::future::pending::<()>().await;
            }
        };
        let (endpoint, conn, ordered_send, ordered_recv) = tokio::select! {
            quic = quic => quic?,
            () = silent => return Err(IceQuicError::QuicSilent(silence_timeout)),
        };
        tracing::debug!(
            controlling,
            rtt = ?conn.rtt(),
            max_datagram = ?conn.max_datagram_size(),
            "ICE+QUIC: connected"
        );

        let max_message_bytes = local.max_message_bytes.min(remote.max_message_bytes) as usize;
        Ok(IceQuicLink::start(LinkParts {
            agent: agent.take(),
            endpoint,
            conn,
            ordered_send,
            ordered_recv,
            max_message_bytes,
            state,
            ice_state,
        }))
    }
}

fn client_tls(
    provider: Arc<CryptoProvider>,
    pin: Arc<Pin>,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<rustls::ClientConfig, IceQuicError> {
    let setup = |error: rustls::Error| IceQuicError::Setup(format!("TLS: {error}"));
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(setup)?
        .dangerous()
        .with_custom_certificate_verifier(pin)
        .with_client_auth_cert(chain, key)
        .map_err(setup)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    Ok(tls)
}

fn server_tls(
    provider: Arc<CryptoProvider>,
    pin: Arc<Pin>,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<rustls::ServerConfig, IceQuicError> {
    let setup = |error: rustls::Error| IceQuicError::Setup(format!("TLS: {error}"));
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(setup)?
        .with_client_cert_verifier(pin)
        .with_single_cert(chain, key)
        .map_err(setup)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    Ok(tls)
}

#[cfg(test)]
mod network_type_tests {
    use super::*;

    #[test]
    fn non_loopback_gathering_names_udp_networks() {
        assert_eq!(network_types(true), vec![NetworkType::Udp4]);
        let all = network_types(false);
        assert!(all.contains(&NetworkType::Udp4));
        assert!(!all.is_empty());
    }
}
