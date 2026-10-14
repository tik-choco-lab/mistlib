//! Exercise retransmitted answers through the browser's real SDP state machine.
use super::*;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test(async)]
async fn cleanup_notifies_established_peers_once_and_never_announces_a_failed_join() {
    use std::cell::RefCell;
    use std::rc::Rc;

    for (watchdog, state, expected) in [
        (false, ConnectionState::Reconnecting, true),
        (false, ConnectionState::Reconnecting, false),
        (true, ConnectionState::Reconnecting, true),
        (true, ConnectionState::Connecting, false),
        (true, ConnectionState::Reconnecting, false),
    ] {
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
        let transport =
            WasmWebRtcTransport::new(Arc::new(Recorder::default()), NodeId("local".into()));
        transport.set_room_id("cleanup-room".into());
        transport.set_ice_servers(vec![]);
        let remote = NodeId("remote".into());
        let peer = transport.create_pc(remote.clone()).unwrap();
        peer.connection_announced.store(expected, Ordering::SeqCst);
        transport
            .peers
            .write()
            .unwrap()
            .insert(remote.clone(), peer.clone());
        transport
            .connection_states
            .write()
            .unwrap()
            .insert(remote.clone(), state);
        if watchdog {
            transport.connection_timeout_ms.store(1, Ordering::Relaxed);
            transport
                .connection_attempt_ids
                .write()
                .unwrap()
                .insert(remote.clone(), 1);
            // The second callback must see the retired attempt and emit nothing.
            for _ in 0..2 {
                transport.spawn_connection_watchdog(
                    remote.clone(),
                    1,
                    peer.clone(),
                    ConnectionWatchdogPhase::Connectivity,
                );
            }
        } else {
            transport.disconnected_since.write().unwrap().insert(
                remote.clone(),
                DisconnectGrace {
                    started_at: web_time::Instant::now()
                        - std::time::Duration::from_millis(DISCONNECTED_GRACE_MS),
                    origin: GraceOrigin::LivenessSuspect,
                },
            );
            transport.ensure_session_sweeper();
        }
        for _ in 0..60 {
            if !transport.peers.read().unwrap().contains_key(&remote) {
                break;
            }
            gloo_timers::future::TimeoutFuture::new(100).await;
        }
        // Includes another sweeper tick and flushes deferred host callbacks.
        gloo_timers::future::TimeoutFuture::new(if watchdog { 10 } else { 2100 }).await;
        transport.stop_session_sweeper();
        transport.close_all_peer_connections();
        crate::app::EVENT_CALLBACK.with(|slot| {
            slot.replace(previous);
        });
        let want = if expected {
            vec![(
                crate::app::EVENT_PEER_DISCONNECTED,
                "remote".into(),
                "cleanup-room".into(),
            )]
        } else {
            vec![]
        };
        assert_eq!(
            *notifications.borrow(),
            want,
            "watchdog={watchdog}, state={state:?}"
        );
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<MessageContent>>);

#[async_trait(?Send)]
impl Signaler for Recorder {
    async fn send_signaling(
        &self,
        _: &NodeId,
        message: MessageContent,
    ) -> mistlib_core::error::Result<()> {
        self.0.lock().unwrap().push(message);
        Ok(())
    }

    async fn close(&self) -> mistlib_core::error::Result<()> {
        Ok(())
    }
}

#[wasm_bindgen_test(async)]
async fn duplicate_answers_are_idempotent_and_acknowledged() {
    let recorder = Arc::new(Recorder::default());
    let transport = WasmWebRtcTransport::new(recorder.clone(), NodeId("a".into()));
    transport.set_ice_servers(vec![]);
    let remote = NodeId("b".into());
    let peer = transport.create_pc(remote.clone()).unwrap();
    transport
        .peers
        .write()
        .unwrap()
        .insert(remote.clone(), peer.clone());
    peer.pc.create_data_channel("reliable");
    let offer = JsFuture::from(peer.pc.create_offer()).await.unwrap();
    let offer: RtcSessionDescriptionInit = offer.unchecked_into();
    JsFuture::from(peer.pc.set_local_description(&offer))
        .await
        .unwrap();
    let answerer = RtcPeerConnection::new().unwrap();
    JsFuture::from(answerer.set_remote_description(&offer))
        .await
        .unwrap();
    let answer = JsFuture::from(answerer.create_answer()).await.unwrap();
    let sdp = Reflect::get(&answer, &JsValue::from_str("sdp"))
        .unwrap()
        .as_string()
        .unwrap();
    let raw = MessageContent::Data(SignalingData {
        sender_id: remote.clone(),
        receiver_id: NodeId("a".into()),
        room_id: String::new(),
        signaling_type: SignalingType::Answer,
        data: sdp.clone(),
    });
    // Raw SDP comes from native. Concurrent delivery via bootstrap and overlay
    // must serialize the precondition check with setRemoteDescription.
    let (first, duplicate) = futures::join!(
        transport.handle_message(raw.clone()),
        transport.handle_message(raw.clone())
    );
    first.unwrap();
    duplicate.expect("concurrent raw answer retransmission");
    // Browsers append trickled candidates to remoteDescription.sdp. Dedupe
    // must still recognize the original answer after that mutation.
    let candidate =
        RtcIceCandidateInit::new("candidate:1 1 UDP 2122260223 127.0.0.1 54321 typ host");
    candidate.set_sdp_mid(Some("0"));
    candidate.set_sdp_m_line_index(Some(0));
    JsFuture::from(
        peer.pc
            .add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&candidate)),
    )
    .await
    .unwrap();
    transport
        .handle_message(raw)
        .await
        .expect("late raw answer retransmission");

    // Native re-reads localDescription on resend after ICE gathering.
    let augmented = format!(
        "{sdp}a=candidate:2 1 UDP 2122260223 127.0.0.1 54322 typ host\r\na=end-of-candidates\r\n"
    );
    let augmented_message = MessageContent::Data(SignalingData {
        sender_id: remote.clone(),
        receiver_id: NodeId("a".into()),
        room_id: String::new(),
        signaling_type: SignalingType::Answer,
        data: augmented.clone(),
    });
    let (first, concurrent) = futures::join!(
        transport.handle_message(augmented_message.clone()),
        transport.handle_message(augmented_message.clone())
    );
    first.expect("candidate-augmented raw answer retransmission");
    concurrent.expect("concurrent candidate-augmented retransmission");
    gloo_timers::future::TimeoutFuture::new(0).await;
    let remote_sdp = peer.pc.remote_description().unwrap().sdp();
    assert!(
        remote_sdp.contains("127.0.0.1 54321 typ host"),
        "keep the earlier trickled candidate"
    );
    assert_eq!(remote_sdp.matches("127.0.0.1 54322 typ host").count(), 1);
    transport.handle_message(augmented_message).await.unwrap();
    gloo_timers::future::TimeoutFuture::new(0).await;
    assert_eq!(peer.pc.remote_description().unwrap().sdp(), remote_sdp);
    for field in ["a=ice-ufrag:", "a=fingerprint:"] {
        assert!(augmented.contains(field));
        let changed = MessageContent::Data(SignalingData {
            sender_id: remote.clone(),
            receiver_id: NodeId("a".into()),
            room_id: String::new(),
            signaling_type: SignalingType::Answer,
            data: augmented.replace(field, &format!("{field}changed")),
        });
        assert!(transport.handle_message(changed).await.is_err());
        assert_eq!(peer.pc.signaling_state(), RtcSignalingState::Stable);
    }

    // A new envelope ID with the same SDP must also be acknowledged, even
    // though the transaction-ID dedupe cache has never seen this ID before.
    let enveloped = MessageContent::Data(SignalingData {
        sender_id: remote.clone(),
        receiver_id: NodeId("a".into()),
        room_id: String::new(),
        signaling_type: SignalingType::Answer,
        data: serde_json::to_string(&NegotiationEnvelope {
            id: 42,
            sdp: augmented.replace("\r\n", "\n"),
        })
        .unwrap(),
    });
    transport
        .handle_message(enveloped)
        .await
        .expect("ack duplicate SDP");
    gloo_timers::future::TimeoutFuture::new(0).await;
    assert!(recorder.0.lock().unwrap().iter().any(|msg| matches!(msg,
        MessageContent::Data(data) if data.signaling_type == SignalingType::NegotiationAck
    )));
    assert_eq!(peer.pc.signaling_state(), RtcSignalingState::Stable);
    assert!(Arc::ptr_eq(
        transport.peers.read().unwrap().get(&remote).unwrap(),
        &peer
    ));
    let unrelated = MessageContent::Data(SignalingData {
        sender_id: remote.clone(),
        receiver_id: NodeId("a".into()),
        room_id: String::new(),
        signaling_type: SignalingType::Answer,
        data: sdp.replace("s=-", "s=unrelated"),
    });
    assert!(transport.handle_message(unrelated).await.is_err());
    assert_eq!(peer.pc.signaling_state(), RtcSignalingState::Stable);
    peer.close_all(&remote);
    answerer.close();
}
