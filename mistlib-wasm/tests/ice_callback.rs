#![cfg(target_arch = "wasm32")]

use async_trait::async_trait;
use js_sys::{Function, Reflect};
use mistlib_core::config::Config;
use mistlib_core::engine::{EngineState, MistEngine, RunningContext};
use mistlib_core::overlay::{ActionHandler, OverlayRouter, OverlayTransport};
use mistlib_core::signaling::{MessageContent, Signaler};
use mistlib_core::transport::Transport;
use mistlib_core::types::{ConnectionState, NodeId};
use mistlib_wasm::runtime::WasmRuntime;
use mistlib_wasm::transport::webrtc::{Peer, WasmWebRtcTransport};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_test::wasm_bindgen_test;
use web_sys::RtcPeerConnection;

struct NoopSignaler;
#[async_trait(?Send)]
impl Signaler for NoopSignaler {
    async fn send_signaling(&self, _: &NodeId, _: MessageContent) -> mistlib_core::Result<()> {
        Ok(())
    }
    async fn close(&self) -> mistlib_core::Result<()> {
        Ok(())
    }
}
struct Actions(Arc<MistEngine>);
impl ActionHandler for Actions {
    fn handle_action(&self, action: mistlib_core::action::OverlayAction) {
        self.0.handle_action(action);
    }
}

// Like backpressure_wait.rs, a browser-shaped PC lets Node run the actual
// production callback without SCTP. The browser unit test additionally covers
// the session registry -> emit_peer_disconnected -> engine notification path.
#[wasm_bindgen_test]
fn terminal_ice_callback_releases_all_guards_before_browser_close_reentry() {
    for terminal in ["failed", "closed"] {
        let local = NodeId("local".into());
        let remote = NodeId("remote".into());
        let room = format!("w18-node-{terminal}");
        let transport = Arc::new(WasmWebRtcTransport::new(
            Arc::new(NoopSignaler),
            local.clone(),
        ));
        let engine = MistEngine::new(Arc::new(WasmRuntime));
        let router = Arc::new(OverlayRouter::new(
            &Config::new_default(),
            engine.node_store.clone(),
            local.clone(),
        ));
        let overlay = Arc::new(OverlayTransport {
            router: router.clone(),
            action_handler: Arc::new(Actions(engine.clone())),
        });
        *engine.state.lock().unwrap() = EngineState::Running(Arc::new(RunningContext {
            transport: overlay,
            network_transport: Some(transport.clone()),
            signaling_handler: transport.clone(),
            p2p_signaling_handler: None,
            signaling_dispatch: None,
            websocket_signaler: None,
            overlay: Some(router),
        }));
        let pc: RtcPeerConnection = Function::new_no_args(
            "return {iceConnectionState: 'connected', close() { this.onTestClose(); }}",
        )
        .call0(&JsValue::NULL)
        .unwrap()
        .unchecked_into();
        let peer = Arc::new(Peer::new(pc));
        peer.setup_handlers(
            remote.clone(),
            transport.signaler.clone(),
            local,
            room,
            transport.connection_states.clone(),
            transport.disconnected_since.clone(),
            transport.event_handler.clone(),
            transport.peers.clone(),
            transport.peer_senders.clone(),
            transport.pending_candidates.clone(),
            transport.candidate_delivery.clone(),
            transport.negotiation_delivery.clone(),
            1,
            16_384,
        );
        transport
            .peers
            .write()
            .unwrap()
            .insert(remote.clone(), peer.clone());
        transport
            .connection_states
            .write()
            .unwrap()
            .insert(remote.clone(), ConnectionState::Connected);
        let closes = Rc::new(Cell::new(0));
        let observed = closes.clone();
        let checking = transport.clone();
        let reentrant_engine = engine.clone();
        let close = Closure::<dyn Fn()>::new(move || {
            // Check before invoking engine code so regressions fail clearly
            // instead of aborting wasm's no_threads lock implementation.
            assert!(checking.connection_states.try_write().is_ok());
            assert!(checking.peers.try_write().unwrap().is_empty());
            assert!(checking.disconnected_since.try_write().is_ok());
            assert!(checking.peer_senders.try_write().is_ok());
            assert!(checking.pending_candidates.try_write().is_ok());
            assert!(checking.candidate_delivery.try_write().is_ok());
            assert!(checking.negotiation_delivery.try_write().is_ok());
            reentrant_engine.notify_peer_disconnected();
            observed.set(observed.get() + 1);
        });
        Reflect::set(&peer.pc, &"onTestClose".into(), close.as_ref()).unwrap();
        let dispatch = Function::new_with_args(
            "pc,state",
            "
            pc.iceConnectionState = state;
            const handler = pc.oniceconnectionstatechange;
            handler({});
            handler({}); // queued stale callback must not tear down twice
        ",
        );
        dispatch
            .call2(&JsValue::NULL, &peer.pc, &terminal.into())
            .unwrap();
        Reflect::delete_property(&peer.pc, &"onTestClose".into()).unwrap();
        assert_eq!(closes.get(), 1);
        assert_eq!(
            transport.get_connection_state(&remote),
            ConnectionState::Disconnected
        );
        assert!(transport.get_connected_nodes().is_empty());
        *engine.state.lock().unwrap() = EngineState::Idle;
    }
}
