//! Certificate pinning: accept exactly the leaf certificate whose SHA-256
//! the peer signaled, for both the server and the client certificate.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of a DER certificate.
pub(crate) fn fingerprint(cert: &CertificateDer<'_>) -> String {
    Sha256::digest(cert.as_ref())
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[derive(Debug)]
pub(crate) struct Pin {
    expected: String,
    provider: Arc<CryptoProvider>,
    /// Set when a presented certificate did not match, so a failed handshake
    /// can be reported as a fingerprint mismatch rather than a generic TLS
    /// error.
    mismatched: AtomicBool,
}

impl Pin {
    pub(crate) fn new(expected: String, provider: Arc<CryptoProvider>) -> Arc<Self> {
        Arc::new(Self {
            expected,
            provider,
            mismatched: AtomicBool::new(false),
        })
    }

    pub(crate) fn mismatched(&self) -> bool {
        self.mismatched.load(Ordering::Relaxed)
    }

    fn verify(&self, cert: &CertificateDer<'_>) -> Result<(), Error> {
        if fingerprint(cert) == self.expected {
            Ok(())
        } else {
            self.mismatched.store(true, Ordering::Relaxed);
            Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }
}

// The fingerprint authenticates the exact leaf DER, not a DNS name or a CA.
// CertificateVerify is still checked: the peer must prove it holds the key.
macro_rules! signature_methods {
    () => {
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    };
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.verify(cert)?;
        Ok(ServerCertVerified::assertion())
    }

    signature_methods!();
}

impl ClientCertVerifier for Pin {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        cert: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.verify(cert)?;
        Ok(ClientCertVerified::assertion())
    }

    signature_methods!();
}
