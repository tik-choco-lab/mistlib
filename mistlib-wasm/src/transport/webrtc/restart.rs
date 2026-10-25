use super::{send_negotiation_reliably, NegotiationDelivery, Peer};
use js_sys::Reflect;
use mistlib_core::signaling::{Signaler, SignalingType};
use mistlib_core::types::NodeId;
use std::sync::{Arc, RwLock};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    RtcIceConnectionState, RtcOfferOptions, RtcSdpType, RtcSessionDescriptionInit,
    RtcSignalingState,
};

/// Fires an ICE-restart offer on `peer`'s existing connection: `createOffer`
/// with `iceRestart: true` -> `setLocalDescription` -> send it through the
/// normal offer signaling envelope. Called from `Peer::setup_handlers`'s
/// `oniceconnectionstatechange` handler when `ice_restart::should_trigger_ice_restart`
/// says so (new disconnected-grace period, we're the initiator, signaling
/// still stable). Deliberately does not go through `renegotiate_peer` --
/// that function rejects a `Disconnected` ice_state on purpose, but this
/// path exists precisely because ice_state just became `Disconnected`.
///
/// Takes `Peer::negotiating` for the same reason `renegotiate_peer` and
/// `apply_offer_in_place` do -- an ICE restart's createOffer/setLocalDescription
/// pair mutates signaling state just like theirs, so it must not race a
/// renegotiation triggered concurrently by, say, a `publish_local_track` call
/// landing at the same moment as the ICE disconnect.
///
/// The remote side receives this as an ordinary `Offer`; since its signaling
/// state is still `Stable`, `handle_offer` takes the `ApplyInPlace` branch
/// and answers it in-place, same as any other renegotiation. A failure here
/// (createOffer/setLocalDescription rejected, or the signaling send fails)
/// is left to the existing disconnected-grace expiry/cleanup as a safety
/// net -- no separate rollback is attempted (unlike `renegotiate_peer`'s),
/// since a stuck-non-Stable state here is subsumed by that same expiry path
/// eventually tearing down and recreating the peer regardless.
pub(super) async fn trigger_ice_restart(
    peer: Arc<Peer>,
    signaler: Arc<dyn Signaler>,
    local_id: NodeId,
    remote_id: NodeId,
    room_id: String,
    negotiation_delivery: Arc<RwLock<NegotiationDelivery>>,
) {
    let _negotiating = peer.negotiating.lock().await;
    let pc = &peer.pc;

    // The browser can recover before this spawned task runs or while it is
    // waiting for another negotiation. Never restart a recovered/closed PC.
    if pc.ice_connection_state() != RtcIceConnectionState::Disconnected {
        tracing::debug!(
            "Skipping ICE restart for {}: ICE no longer disconnected",
            remote_id.0
        );
        return;
    }
    if pc.signaling_state() != RtcSignalingState::Stable {
        tracing::debug!(
            "Skipping ICE restart for {}: signaling state no longer stable",
            remote_id.0
        );
        return;
    }

    let options = RtcOfferOptions::new();
    options.set_ice_restart(true);
    let offer = match JsFuture::from(pc.create_offer_with_rtc_offer_options(&options)).await {
        Ok(offer) => offer,
        Err(err) => {
            tracing::warn!(
                "ICE restart createOffer failed for {}: {:?}",
                remote_id.0,
                err
            );
            return;
        }
    };
    let sdp = match Reflect::get(&offer, &JsValue::from_str("sdp"))
        .ok()
        .and_then(|v| v.as_string())
    {
        Some(sdp) => sdp,
        None => {
            tracing::warn!("ICE restart offer for {} had no SDP field", remote_id.0);
            return;
        }
    };

    // createOffer is asynchronous too. Recovery can win that await even when
    // the peer was disconnected after we acquired the lock. Until setLocal-
    // Description, we can still discard the stale proposal without committing
    // another negotiation. After commit, the exchange must run to completion.
    if pc.ice_connection_state() != RtcIceConnectionState::Disconnected {
        tracing::debug!(
            "Skipping ICE restart for {}: ICE recovered during createOffer",
            remote_id.0
        );
        return;
    }

    let sdp_init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    sdp_init.set_sdp(&sdp);
    if let Err(err) = JsFuture::from(pc.set_local_description(&sdp_init)).await {
        tracing::warn!(
            "ICE restart setLocalDescription failed for {}: {:?}",
            remote_id.0,
            err
        );
        return;
    }

    tracing::info!("Sending ICE restart offer to {}", remote_id.0);
    send_negotiation_reliably(
        signaler,
        local_id,
        remote_id,
        room_id,
        negotiation_delivery,
        SignalingType::Offer,
        sdp,
    )
    .await;
}
