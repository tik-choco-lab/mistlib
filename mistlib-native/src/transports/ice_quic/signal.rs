//! Signaling payload of the ICE+QUIC link (camelCase JSON).

use serde::{Deserialize, Serialize};

use super::IceQuicError;

/// Version of the payload and of the link protocol it negotiates (the
/// framing and stream layout behind ALPN `mistlib-iq1`).
pub(crate) const PROTOCOL_VERSION: &str = "iq1";

/// Bounds on what a remote payload may contain. The candidate cap mirrors
/// the WebRTC transport's `MAX_PENDING_CANDIDATES_PER_NODE`.
const MAX_CANDIDATES: usize = super::super::webrtc::MAX_PENDING_CANDIDATES_PER_NODE;
const MAX_CANDIDATE_LEN: usize = 512;
const MAX_CREDENTIAL_LEN: usize = 256;

/// One side's ICE and QUIC parameters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IceQuicParams {
    /// [`PROTOCOL_VERSION`]. A different value is rejected.
    pub version: String,
    pub ice_ufrag: String,
    pub ice_pwd: String,
    /// Every local candidate, in `candidate:` attribute syntax without the
    /// `a=` / `candidate:` prefix handling of SDP (webrtc-ice `marshal`).
    /// Complete: gathering finished (or timed out) before signaling.
    pub candidates: Vec<String>,
    /// SHA-256 of the sender's DER certificate, lowercase hex (64 chars).
    pub cert_sha256: String,
    /// Largest reliable message the sender accepts.
    pub max_message_bytes: u32,
}

impl IceQuicParams {
    /// Rejects a payload this side cannot use, before any ICE work.
    pub(crate) fn validate(&self) -> Result<(), IceQuicError> {
        let fail = |what: &str| Err(IceQuicError::Signal(what.to_string()));
        if self.version != PROTOCOL_VERSION {
            return Err(IceQuicError::Signal(format!(
                "unsupported version {:?} (expected {PROTOCOL_VERSION:?})",
                self.version
            )));
        }
        if self.ice_ufrag.is_empty() || self.ice_ufrag.len() > MAX_CREDENTIAL_LEN {
            return fail("bad ICE ufrag");
        }
        if self.ice_pwd.is_empty() || self.ice_pwd.len() > MAX_CREDENTIAL_LEN {
            return fail("bad ICE pwd");
        }
        if self.candidates.is_empty() {
            return fail("no candidates");
        }
        if self.candidates.len() > MAX_CANDIDATES {
            return fail("too many candidates");
        }
        if self.candidates.iter().any(|c| c.len() > MAX_CANDIDATE_LEN) {
            return fail("candidate too long");
        }
        if !is_sha256_hex(&self.cert_sha256) {
            return fail("certificate fingerprint is not 64 lowercase hex digits");
        }
        if self.max_message_bytes == 0 {
            return fail("maxMessageBytes is zero");
        }
        Ok(())
    }
}

fn is_sha256_hex(value: &str) -> bool {
    let lower_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    value.len() == 64 && value.bytes().all(lower_hex)
}

/// The offerer's payload. Its sender becomes ICE-controlling and the QUIC
/// client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct IceQuicOffer(pub IceQuicParams);

/// The answerer's payload. Same wire shape as the offer; the signaling
/// message type tells them apart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct IceQuicAnswer(pub IceQuicParams);
