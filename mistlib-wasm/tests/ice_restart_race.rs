#![cfg(target_arch = "wasm32")]

use async_trait::async_trait;
use js_sys::{Function, Reflect};
use mistlib_core::signaling::{MessageContent, Signaler, SignalingType};
use mistlib_core::types::NodeId;
use mistlib_wasm::transport::webrtc::{negotiation_delivery::NegotiationDelivery, Peer};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::wasm_bindgen_test;
use web_sys::RtcPeerConnection;

// Compile the actual async restart path; replace only its signaling delivery
// endpoint so this test needs neither a browser connection nor retry timers.
#[path = "../src/transport/webrtc/restart.rs"]
mod restart;

async fn send_negotiation_reliably(
    signaler: Arc<dyn Signaler>,
    _: NodeId,
    remote: NodeId,
    _: String,
    _: Arc<RwLock<NegotiationDelivery>>,
    kind: SignalingType,
    sdp: String,
) {
    assert_eq!(kind, SignalingType::Offer);
    assert_eq!(sdp, "restart offer");
    signaler
        .send_signaling(&remote, MessageContent::Raw(bytes::Bytes::new()))
        .await
        .unwrap();
}

#[derive(Default)]
struct Signals(AtomicUsize);

#[async_trait(?Send)]
impl Signaler for Signals {
    async fn send_signaling(
        &self,
        _: &NodeId,
        _: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    async fn close(&self) -> mistlib_core::error::Result<()> {
        Ok(())
    }
}

fn mock_peer(ice_state: &str) -> Arc<Peer> {
    let pc: RtcPeerConnection = Function::new_no_args(
        "return {iceConnectionState: 'disconnected', signalingState: 'stable', offers: 0, locals: 0,
         createOffer(options) {if (!options.iceRestart) throw Error('expected restart');
           this.offers++; return Promise.resolve().then(() => {
             if (this.recoverInOffer) this.iceConnectionState = 'connected';
             return {sdp: 'restart offer'};});},
         setLocalDescription(sdp) {this.locals++; return Promise.resolve();}}"
    ).call0(&JsValue::NULL).unwrap().unchecked_into();
    Reflect::set(&pc, &"iceConnectionState".into(), &ice_state.into()).unwrap();
    Arc::new(Peer::new(pc))
}

async fn trigger(peer: Arc<Peer>, signals: Arc<Signals>) {
    restart::trigger_ice_restart(
        peer,
        signals,
        NodeId("a".into()),
        NodeId("z".into()),
        "main".into(),
        Arc::new(RwLock::new(NegotiationDelivery::default())),
    )
    .await;
}

fn calls(peer: &Peer, property: &str) -> f64 {
    Reflect::get(&peer.pc, &property.into())
        .unwrap()
        .as_f64()
        .unwrap()
}

#[wasm_bindgen_test]
async fn spawned_restart_checks_current_ice_before_mutating_sdp() {
    for state in [
        "connected",
        "completed",
        "closed",
        "failed",
        "checking",
        "new",
        "disconnected",
    ] {
        let peer = mock_peer(state);
        let signals = Arc::new(Signals::default());
        trigger(peer.clone(), signals.clone()).await;
        let expected = usize::from(state == "disconnected");
        assert_eq!(calls(&peer, "offers"), expected as f64, "{state}");
        assert_eq!(calls(&peer, "locals"), expected as f64, "{state}");
        assert_eq!(signals.0.load(Ordering::Relaxed), expected, "{state}");
    }
}

#[wasm_bindgen_test]
async fn recovery_while_waiting_for_negotiation_lock_skips_restart() {
    let peer = mock_peer("disconnected");
    let signals = Arc::new(Signals::default());
    let guard = peer.negotiating.lock().await;
    let mut restart = Box::pin(trigger(peer.clone(), signals.clone()));
    assert!(futures::poll!(restart.as_mut()).is_pending());
    Reflect::set(&peer.pc, &"iceConnectionState".into(), &"connected".into()).unwrap();
    drop(guard);
    restart.await;
    assert_eq!(calls(&peer, "offers"), 0.0);
    assert_eq!(calls(&peer, "locals"), 0.0);
    assert_eq!(signals.0.load(Ordering::Relaxed), 0);
}

#[wasm_bindgen_test]
async fn recovery_during_create_offer_discards_the_uncommitted_restart() {
    let peer = mock_peer("disconnected");
    let signals = Arc::new(Signals::default());
    Reflect::set(&peer.pc, &"recoverInOffer".into(), &JsValue::TRUE).unwrap();
    trigger(peer.clone(), signals.clone()).await;
    assert_eq!(calls(&peer, "offers"), 1.0);
    assert_eq!(calls(&peer, "locals"), 0.0);
    assert_eq!(signals.0.load(Ordering::Relaxed), 0);
}
