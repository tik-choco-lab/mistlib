//! Drive the installed ICE callback with terminal browser state values and the
//! same session -> engine -> network_transport wiring used by build_session.
use super::*;
use mistlib_core::config::Config;
use mistlib_core::engine::{EngineState, MistEngine, RunningContext};
use mistlib_core::overlay::{ActionHandler, OverlayRouter, OverlayTransport};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen_test::wasm_bindgen_test;

#[derive(Default)]
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

#[wasm_bindgen_test]
async fn failed_and_closed_ice_callbacks_can_reenter_the_running_engine() {
    for terminal in ["failed", "closed"] {
        let room = format!("w18-ice-{terminal}");
        let local = NodeId("local".into());
        let remote = NodeId("remote".into());
        let engine = MistEngine::new(Arc::new(crate::runtime::WasmRuntime));
        let router = Arc::new(OverlayRouter::new(
            &Config::new_default(),
            engine.node_store.clone(),
            local.clone(),
        ));
        let overlay = Arc::new(OverlayTransport {
            router: router.clone(),
            action_handler: Arc::new(Actions(engine.clone())),
        });
        let transport = Arc::new(WasmWebRtcTransport::new(
            Arc::new(NoopSignaler),
            local.clone(),
        ));
        transport.set_room_id(room.clone());
        transport.set_ice_servers(vec![]);
        let ctx = Arc::new(RunningContext {
            transport: overlay.clone(),
            network_transport: Some(transport.clone()),
            signaling_handler: transport.clone(),
            p2p_signaling_handler: None,
            signaling_dispatch: None,
            websocket_signaler: None,
            overlay: Some(router),
        });
        *engine.state.lock().unwrap() = EngineState::Running(ctx);
        crate::app::insert_session(
            room.clone(),
            crate::app::Session {
                engine: engine.clone(),
                webrtc: transport.clone(),
                l1_transport: Arc::new(crate::layers::wasm_l1::WasmL1Transport::new(
                    overlay,
                    engine.node_store.clone(),
                    local,
                )),
            },
        );
        let peer = transport.create_pc(remote.clone()).unwrap();
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
        peer.connection_announced.store(true, Ordering::SeqCst);
        let dc = peer.pc.create_data_channel("reliable");
        peer.channels
            .write()
            .unwrap()
            .insert(DeliveryMethod::ReliableOrdered, dc);

        let notifications = Rc::new(RefCell::new(Vec::new()));
        let received = notifications.clone();
        let callback = Closure::<dyn FnMut(u32, String, js_sys::Uint8Array, String)>::new(
            move |kind, node, _, room| received.borrow_mut().push((kind, node, room)),
        );
        let previous = crate::app::EVENT_CALLBACK.with(|slot| {
            slot.replace(Some(
                callback
                    .as_ref()
                    .unchecked_ref::<js_sys::Function>()
                    .clone(),
            ))
        });
        let want = crate::storage::WANT_REGISTRY.with(|registry| {
            let want = registry.register_attempt("w18-callback", remote.clone());
            registry.track_path(want.id, &room);
            want
        });
        // Browsers expose ICE state as read-only. Inject the terminal state
        // on this real PC, then invoke its production event handler. No network
        // timing or a test-only replacement of Rust callback logic is involved.
        let dispatch = js_sys::Function::new_with_args(
            "pc,state",
            "
            Object.defineProperty(pc, 'iceConnectionState', {configurable: true, value: state});
            const handler = pc.oniceconnectionstatechange;
            handler(new Event('iceconnectionstatechange'));
            return handler;
        ",
        );
        let old_handler = dispatch
            .call2(&JsValue::NULL, &peer.pc, &terminal.into())
            .unwrap();
        assert_eq!(
            transport.get_connection_state(&remote),
            ConnectionState::Disconnected
        );
        assert!(transport.get_connected_nodes().is_empty());
        assert!(transport.peers.try_write().unwrap().is_empty());
        assert!(transport.peer_senders.try_write().unwrap().is_empty());
        assert!(transport.disconnected_since.try_write().unwrap().is_empty());
        assert!(transport.pending_candidates.try_write().is_ok());
        assert!(transport.candidate_delivery.try_write().is_ok());
        assert!(transport.negotiation_delivery.try_write().is_ok());
        assert!(peer.channels.try_write().unwrap().is_empty());
        assert_eq!(peer.pc.signaling_state(), RtcSignalingState::Closed);
        assert_eq!(*want.resets.borrow(), 1);
        // Queued stale callbacks must not deliver a second notification/reset.
        old_handler
            .unchecked_into::<js_sys::Function>()
            .call1(
                &JsValue::NULL,
                &web_sys::Event::new("iceconnectionstatechange").unwrap(),
            )
            .unwrap();
        assert_eq!(*want.resets.borrow(), 1);
        // The engine notification above has already synchronously read the
        // network transport; JS host notifications are deferred one microtask.
        gloo_timers::future::TimeoutFuture::new(0).await;
        assert_eq!(
            *notifications.borrow(),
            vec![(
                crate::app::EVENT_PEER_DISCONNECTED,
                remote.0.clone(),
                room.clone()
            )]
        );
        crate::app::EVENT_CALLBACK.with(|slot| {
            slot.replace(previous);
        });
        crate::app::remove_session(&room);
        *engine.state.lock().unwrap() = EngineState::Idle;
        transport.close_all_peer_connections();
        drop(want.data);
        crate::storage::WANT_REGISTRY.with(|registry| registry.cancel_closed());
    }
}
