//! Browser regression tests: inject the reported rejection, exercise the real
//! Rust handler, and let Chromium apply the retried SDP on the replacement.
use super::*;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen(inline_js = "
export function rejectOffer(pc, roleError, rejectRollback) {
  const local = pc.setLocalDescription.bind(pc);
  pc.setRemoteDescription = () => Promise.reject(new DOMException(
    roleError ? 'Failed to set SSL role for the transport.' : 'Invalid SDP', 'OperationError'));
  if (rejectRollback) pc.setLocalDescription = desc => {
    if (desc.type !== 'rollback') return local(desc);
    if (pc.onTestRollback) pc.onTestRollback();
    return Promise.reject(new DOMException('Injected rollback failure', 'OperationError'));
  };
}
")]
extern "C" {
    #[wasm_bindgen(js_name = rejectOffer)]
    fn reject_offer(pc: &RtcPeerConnection, role_error: bool, reject_rollback: bool);
}

#[derive(Default)]
struct RecordingSignaler(Mutex<Vec<SignalingData>>);

#[async_trait(?Send)]
impl Signaler for RecordingSignaler {
    async fn send_signaling(
        &self,
        _: &NodeId,
        msg: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        if let MessageContent::Data(data) = msg {
            self.0.lock().unwrap().push(data);
        }
        Ok(())
    }
    async fn close(&self) -> mistlib_core::error::Result<()> {
        Ok(())
    }
}

async fn local_offer(pc: &RtcPeerConnection) -> String {
    pc.create_data_channel("reliable");
    let offer = JsFuture::from(pc.create_offer()).await.unwrap();
    let sdp = Reflect::get(&offer, &JsValue::from_str("sdp"))
        .unwrap()
        .as_string()
        .unwrap();
    let desc = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    desc.set_sdp(&sdp);
    JsFuture::from(pc.set_local_description(&desc))
        .await
        .unwrap();
    sdp
}

async fn check_failure(role_error: bool, reject_rollback: bool, rebuild: bool, inject: bool) {
    let signaler = Arc::new(RecordingSignaler::default());
    let transport = WasmWebRtcTransport::new(signaler.clone(), NodeId("local".into()));
    transport.set_ice_servers(vec![]);
    let remote = NodeId("remote".into());
    let old = transport.create_pc(remote.clone()).unwrap();
    transport
        .peers
        .write()
        .unwrap()
        .insert(remote.clone(), old.clone());
    transport
        .connection_states
        .write()
        .unwrap()
        .insert(remote.clone(), ConnectionState::Connecting);
    transport
        .peer_epochs
        .write()
        .unwrap()
        .insert(remote.clone(), 42);
    local_offer(&old.pc).await;
    let sender = RtcPeerConnection::new().unwrap();
    let offer = local_offer(&sender).await;
    if inject {
        reject_offer(&old.pc, role_error, reject_rollback);
    }
    let isolation_epoch = transport.isolation_recovery_epoch.load(Ordering::SeqCst);
    let result = transport.handle_offer(remote.clone(), offer).await;
    let current = transport
        .peers
        .read()
        .unwrap()
        .get(&remote)
        .unwrap()
        .clone();
    if rebuild {
        assert_eq!(result.unwrap(), true);
        assert!(!Arc::ptr_eq(&old, &current));
        assert_eq!(old.pc.signaling_state(), RtcSignalingState::Closed);
        assert_eq!(current.pc.signaling_state(), RtcSignalingState::Stable);
        assert_eq!(
            transport.peer_epochs.read().unwrap().get(&remote),
            Some(&42)
        );
        assert_eq!(
            transport.isolation_recovery_epoch.load(Ordering::SeqCst),
            isolation_epoch
        );
        let answer = signaler
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|data| data.signaling_type == SignalingType::Answer)
            .unwrap()
            .data
            .clone();
        let desc = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        let envelope: NegotiationEnvelope = serde_json::from_str(&answer).unwrap();
        desc.set_sdp(&envelope.sdp);
        JsFuture::from(sender.set_remote_description(&desc))
            .await
            .unwrap();
        assert_eq!(sender.signaling_state(), RtcSignalingState::Stable);
    } else {
        if inject {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap());
        }
        assert!(Arc::ptr_eq(&old, &current));
        assert_ne!(old.pc.signaling_state(), RtcSignalingState::Closed);
    }
    transport.close_all_peer_connections();
    sender.close();
}

#[wasm_bindgen_test]
async fn double_role_failure_retries_offer_on_fresh_pc() {
    check_failure(true, true, true, true).await;
}

#[wasm_bindgen_test]
async fn successful_rollback_preserves_peer() {
    check_failure(true, false, false, true).await;
}

#[wasm_bindgen_test]
async fn unrelated_error_preserves_peer_even_if_rollback_fails() {
    check_failure(false, true, false, true).await;
}

#[wasm_bindgen_test]
async fn normal_crossed_offer_preserves_peer() {
    check_failure(false, false, false, false).await;
}

#[wasm_bindgen_test]
async fn concurrent_replacement_is_not_destroyed_by_old_failure() {
    let transport = WasmWebRtcTransport::new(
        Arc::new(RecordingSignaler::default()),
        NodeId("local".into()),
    );
    transport.set_ice_servers(vec![]);
    let remote = NodeId("remote".into());
    let old = transport.create_pc(remote.clone()).unwrap();
    let replacement = transport.create_pc(remote.clone()).unwrap();
    transport
        .peers
        .write()
        .unwrap()
        .insert(remote.clone(), old.clone());
    local_offer(&old.pc).await;
    let sender = RtcPeerConnection::new().unwrap();
    let offer = local_offer(&sender).await;
    reject_offer(&old.pc, true, true);
    let peers = transport.peers.clone();
    let new_peer = replacement.clone();
    let node = remote.clone();
    let hook = Closure::<dyn Fn()>::new(move || {
        peers
            .write()
            .unwrap()
            .insert(node.clone(), new_peer.clone());
    });
    Reflect::set(&old.pc, &JsValue::from_str("onTestRollback"), hook.as_ref()).unwrap();
    assert!(!transport.handle_offer(remote.clone(), offer).await.unwrap());
    assert!(Arc::ptr_eq(
        transport.peers.read().unwrap().get(&remote).unwrap(),
        &replacement
    ));
    assert_eq!(replacement.pc.signaling_state(), RtcSignalingState::Stable);
    Reflect::delete_property(&old.pc, &JsValue::from_str("onTestRollback")).unwrap();
    old.close_all(&remote);
    transport.close_all_peer_connections();
    sender.close();
}
