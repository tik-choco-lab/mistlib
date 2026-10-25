#![cfg(target_arch = "wasm32")]

use bytes::Bytes;
use futures::{join, FutureExt};
use gloo_timers::future::TimeoutFuture;
use js_sys::{Array, Function, Reflect};
use mistlib_core::types::{DeliveryMethod, NodeId};
use mistlib_wasm::transport::webrtc::Peer;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::wasm_bindgen_test;
use web_sys::{RtcDataChannel, RtcPeerConnection};

// Browser-shaped objects let Node execute the actual gate/drain/FIFO code
// without SCTP or a live browser. No bufferedamountlow event is generated.
fn congested_peer() -> (Peer, RtcDataChannel, Array) {
    let pc: RtcPeerConnection = Function::new_no_args("return {close() {}}")
        .call0(&JsValue::NULL)
        .unwrap()
        .unchecked_into();
    let dc: RtcDataChannel = Function::new_no_args(
        "return {readyState: 'open', label: 'test', bufferedAmount: 1048577, bufferedAmountLowThreshold: 262144, \
         writes: [], send(data) {this.writes.push(Array.from(data));}, \
         close() {this.readyState = 'closed';}}",
    )
    .call0(&JsValue::NULL)
    .unwrap()
    .unchecked_into();
    let writes: Array = Reflect::get(&dc, &"writes".into())
        .unwrap()
        .unchecked_into();
    let peer = Peer::new(pc);
    peer.channels
        .write()
        .unwrap()
        .insert(DeliveryMethod::ReliableOrdered, dc.clone());
    for frame in [b"older".as_slice(), b"newer".as_slice()] {
        assert!(!peer
            .send_queue
            .lock()
            .unwrap()
            .push(Bytes::copy_from_slice(frame)));
    }
    (peer, dc, writes)
}

#[wasm_bindgen_test]
async fn timer_rechecks_capacity_and_preserves_fifo_without_low_events_or_pongs() {
    let (peer, dc, writes) = congested_peer();
    let node = NodeId("remote".into());
    let flush = peer.flush_send_queue(&node);
    let drain = async {
        TimeoutFuture::new(3200).await; // past the first 3 s timer expiry
        assert_eq!(writes.length(), 0, "timer must not permit a congested send");
        assert_eq!(
            peer.send_queue.lock().unwrap().front().unwrap().as_ref(),
            b"older"
        );
        assert!(peer.reliable_send_gate.try_lock().is_err());
        Reflect::set(&dc, &"bufferedAmount".into(), &JsValue::from(1024 * 1024)).unwrap();
    };
    join!(flush, drain);
    assert_eq!(writes.length(), 2);
    assert_eq!(
        Array::from(&writes.get(0)).get(0).as_f64(),
        Some(b'o' as f64)
    );
    assert_eq!(
        Array::from(&writes.get(1)).get(0).as_f64(),
        Some(b'n' as f64)
    );
    assert!(peer.send_queue.lock().unwrap().is_empty());
    assert!(peer.reliable_send_gate.try_lock().is_ok());
}

#[wasm_bindgen_test]
async fn close_wakes_a_drain_wait_and_cancellation_retains_the_fifo_head() {
    let (peer, dc, writes) = congested_peer();
    let node = NodeId("remote".into());
    {
        let mut flush = Box::pin(peer.flush_send_queue(&node));
        assert!(futures::poll!(flush.as_mut()).is_pending());
        // Drop is the existing caller cancellation path; nothing was written.
    }
    assert!(peer.reliable_send_gate.try_lock().is_ok());
    assert_eq!(writes.length(), 0);
    assert_eq!(
        peer.send_queue.lock().unwrap().front().unwrap().as_ref(),
        b"older"
    );
    let mut flush = Box::pin(peer.flush_send_queue(&node));
    assert!(futures::poll!(flush.as_mut()).is_pending());
    // Channel close wake, as the permanent onclose callback does. A failed
    // flush retains both frames; explicit peer teardown clears them later.
    dc.close();
    peer.buffered_amount_waiters.write().unwrap().clear();
    assert!(flush.as_mut().now_or_never().is_some());
    drop(flush);
    assert_eq!(writes.length(), 0);
    assert_eq!(
        peer.send_queue.lock().unwrap().front().unwrap().as_ref(),
        b"older"
    );
    // Peer teardown also wakes an active wait immediately and releases the gate.
    Reflect::set(&dc, &"readyState".into(), &"open".into()).unwrap();
    let mut flush = Box::pin(peer.flush_send_queue(&node));
    assert!(futures::poll!(flush.as_mut()).is_pending());
    peer.close_all(&node);
    assert!(flush.as_mut().now_or_never().is_some());
    drop(flush);
    assert!(peer.send_queue.lock().unwrap().is_empty());
    assert!(peer.reliable_send_gate.try_lock().is_ok());
    assert_eq!(writes.length(), 0);
}

#[wasm_bindgen_test]
async fn split_bulk_gate_allows_control_while_bulk_waits_for_drain() {
    use async_trait::async_trait;
    use mistlib_core::action::SendLane;
    use mistlib_core::signaling::{MessageContent, Signaler};
    use mistlib_core::transport::Transport;
    use mistlib_wasm::transport::webrtc::WasmWebRtcTransport;
    use std::sync::Arc;
    struct NoSignaler;
    #[async_trait(?Send)]
    impl Signaler for NoSignaler {
        async fn send_signaling(
            &self,
            _: &NodeId,
            _: MessageContent,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        async fn close(&self) -> mistlib_core::error::Result<()> {
            Ok(())
        }
    }
    let transport = WasmWebRtcTransport::new(Arc::new(NoSignaler), NodeId("sender".into()));
    transport.set_storage_transfer(mistlib_core::config::StorageTransferConfig {
        unsequenced_bulk: false,
        control_priority: true,
        bulk_channel: false,
        bulk_target_delay_ms: 0,
        ..Default::default()
    });
    let (peer, dc, writes) = congested_peer();
    peer.clear_send_queue();
    // Bulk leaves 64 KiB of stack headroom for the control writer.
    Reflect::set(dc.as_ref(), &"bufferedAmount".into(), &1_020_000u32.into()).unwrap();
    let node = NodeId("receiver".into());
    peer.channels
        .write()
        .unwrap()
        .insert(DeliveryMethod::ReliableOrdered, dc.clone());
    let peer = Arc::new(peer);
    transport
        .peers
        .write()
        .unwrap()
        .insert(node.clone(), peer.clone());
    let cid = mistlib_core::storage::compute_cid(b"data", 0x55);
    let envelope = mistlib_core::overlay::OverlayEnvelope::new(
        NodeId("sender".into()),
        node.clone(),
        1,
        MessageContent::Raw(
            mistlib_core::storage::protocol::build_have_chunk_message(&cid, 0, 1, b"data").into(),
        ),
    );
    let bulk = transport.send_in_lane(
        &node,
        mistlib_core::overlay::wire::serialize(&envelope)
            .unwrap()
            .into(),
        DeliveryMethod::ReliableOrdered,
        SendLane::Bulk,
        true,
    );
    let control = async {
        TimeoutFuture::new(20).await;
        transport
            .send_in_lane(
                &node,
                Bytes::from_static(b"control"),
                DeliveryMethod::ReliableOrdered,
                SendLane::Control,
                true,
            )
            .await
            .unwrap();
        assert_eq!(writes.length(), 1, "control writes while bulk is suspended");
        Reflect::set(dc.as_ref(), &"bufferedAmount".into(), &0u32.into()).unwrap();
        if let Some(waiters) = peer
            .buffered_amount_waiters
            .write()
            .unwrap()
            .remove(&DeliveryMethod::ReliableOrdered)
        {
            for waiter in waiters {
                let _ = waiter.send(());
            }
        }
    };
    let (bulk, ()) = join!(bulk, control);
    bulk.unwrap();
    assert_eq!(writes.length(), 2);
}

#[wasm_bindgen_test]
async fn optional_bulk_channel_falls_back_after_close_without_removing_peer() {
    optional_bulk_case(0).await;
}

#[wasm_bindgen_test]
async fn paced_optional_bulk_channel_preserves_close_and_fallback() {
    optional_bulk_case(100).await;
}

async fn optional_bulk_case(target_delay_ms: u32) {
    use async_trait::async_trait;
    use mistlib_core::{
        action::SendLane,
        signaling::{MessageContent, Signaler},
        transport::Transport,
    };
    use mistlib_wasm::transport::webrtc::WasmWebRtcTransport;
    use std::sync::Arc;
    struct NoSignaler;
    #[async_trait(?Send)]
    impl Signaler for NoSignaler {
        async fn send_signaling(
            &self,
            _: &NodeId,
            _: MessageContent,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        async fn close(&self) -> mistlib_core::error::Result<()> {
            Ok(())
        }
    }
    let transport = WasmWebRtcTransport::new(Arc::new(NoSignaler), NodeId("a".into()));
    let config = mistlib_core::config::StorageTransferConfig {
        bulk_channel: true,
        bulk_target_delay_ms: target_delay_ms,
        ..Default::default()
    };
    transport.set_storage_transfer(config.clone());
    let (peer, reliable, reliable_writes) = congested_peer();
    peer.configure_bulk_pacing(&config);
    peer.clear_send_queue();
    Reflect::set(reliable.as_ref(), &"bufferedAmount".into(), &0u32.into()).unwrap();
    let bulk: RtcDataChannel = Function::new_no_args("return {label:'reliable-unordered', readyState:'open', bufferedAmount:0, writes:[], send(data){this.writes.push(Array.from(data));}, close(){this.readyState='closed';}}")
        .call0(&JsValue::NULL).unwrap().unchecked_into();
    let bulk_writes: Array = Reflect::get(bulk.as_ref(), &"writes".into())
        .unwrap()
        .unchecked_into();
    peer.unordered_capability.set_local(true);
    peer.unordered_capability.accept_remote(Some(1));
    *peer.unordered_channel.write().unwrap() = Some(bulk.clone());
    let node = NodeId("b".into());
    let peer = Arc::new(peer);
    transport
        .peers
        .write()
        .unwrap()
        .insert(node.clone(), peer.clone());
    transport.connection_states.write().unwrap().insert(
        node.clone(),
        mistlib_core::types::ConnectionState::Connected,
    );
    Peer::setup_dc_handlers(
        bulk.clone(),
        DeliveryMethod::ReliableUnordered,
        peer.clone(),
        transport.event_handler.clone(),
        node.clone(),
        "room".into(),
        transport.connection_states.clone(),
        transport.disconnected_since.clone(),
        transport.peers.clone(),
        transport.peer_senders.clone(),
        transport.pending_candidates.clone(),
        65536,
    );
    let cid = mistlib_core::storage::compute_cid(b"data", 0x55);
    let frame = mistlib_core::overlay::OverlayEnvelope::new(
        NodeId("a".into()),
        node.clone(),
        1,
        MessageContent::Raw(
            mistlib_core::storage::protocol::build_have_chunk_message(&cid, 0, 1, b"data").into(),
        ),
    );
    transport
        .send_in_lane(
            &node,
            mistlib_core::overlay::wire::serialize(&frame)
                .unwrap()
                .into(),
            DeliveryMethod::ReliableUnordered,
            SendLane::Bulk,
            true,
        )
        .await
        .unwrap();
    assert_eq!((bulk_writes.length(), reliable_writes.length()), (1, 0));
    transport
        .send_in_lane(
            &node,
            mistlib_core::overlay::wire::serialize(&frame.clone().with_seq(9))
                .unwrap()
                .into(),
            DeliveryMethod::ReliableOrdered,
            SendLane::Bulk,
            true,
        )
        .await
        .unwrap();
    assert_eq!((bulk_writes.length(), reliable_writes.length()), (1, 1));
    // A send-time InvalidStateError also guarantees that no bytes were accepted.
    let original_send = Reflect::get(bulk.as_ref(), &"send".into()).unwrap();
    Reflect::set(bulk.as_ref(), &"send".into(), &Function::new_no_args(
        "throw Object.assign(new Error('closed before submission'), {name:'InvalidStateError'});"
    )).unwrap();
    transport
        .send_wait(
            &node,
            mistlib_core::overlay::wire::serialize(&frame)
                .unwrap()
                .into(),
            DeliveryMethod::ReliableUnordered,
        )
        .await
        .unwrap();
    assert_eq!((bulk_writes.length(), reliable_writes.length()), (1, 2));
    Reflect::set(bulk.as_ref(), &"send".into(), &original_send).unwrap();
    // Close after selection while the writer is suspended, before submission.
    Reflect::set(
        bulk.as_ref(),
        &"bufferedAmount".into(),
        &1_100_000u32.into(),
    )
    .unwrap();
    peer.send_queue
        .lock()
        .unwrap()
        .push(Bytes::from_static(b"deferred-reliable"));
    let in_flight = transport.send_wait(
        &node,
        mistlib_core::overlay::wire::serialize(&frame)
            .unwrap()
            .into(),
        DeliveryMethod::ReliableUnordered,
    );
    futures::pin_mut!(in_flight);
    assert!(futures::poll!(in_flight.as_mut()).is_pending());
    assert_eq!(bulk_writes.length(), 1);
    bulk.close();
    let onclose: Function = Reflect::get(bulk.as_ref(), &"onclose".into())
        .unwrap()
        .unchecked_into();
    onclose
        .call1(bulk.as_ref(), &js_sys::Object::new())
        .unwrap();
    in_flight.await.unwrap();
    assert_eq!((bulk_writes.length(), reliable_writes.length()), (1, 4));
    assert!(peer.send_queue.lock().unwrap().is_empty());
    assert!(peer.unordered_channel.read().unwrap().is_none());
    assert!(transport.peers.read().unwrap().contains_key(&node));
    assert_eq!(
        transport.get_connection_state(&node),
        mistlib_core::types::ConnectionState::Connected
    );
    transport
        .send_in_lane(
            &node,
            mistlib_core::overlay::wire::serialize(&frame)
                .unwrap()
                .into(),
            DeliveryMethod::ReliableUnordered,
            SendLane::Bulk,
            true,
        )
        .await
        .unwrap();
    assert_eq!((bulk_writes.length(), reliable_writes.length()), (1, 5));
}

#[wasm_bindgen_test]
async fn unreliable_send_wait_never_flushes_deferred_reliable_frames() {
    use async_trait::async_trait;
    use mistlib_core::{
        signaling::{MessageContent, Signaler},
        transport::Transport,
    };
    use mistlib_wasm::transport::webrtc::WasmWebRtcTransport;
    use std::sync::Arc;
    struct NoSignaler;
    #[async_trait(?Send)]
    impl Signaler for NoSignaler {
        async fn send_signaling(
            &self,
            _: &NodeId,
            _: MessageContent,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        async fn close(&self) -> mistlib_core::error::Result<()> {
            Ok(())
        }
    }
    for method in [
        DeliveryMethod::Unreliable,
        DeliveryMethod::UnreliableOrdered,
    ] {
        let transport = WasmWebRtcTransport::new(Arc::new(NoSignaler), NodeId("a".into()));
        let (peer, _, reliable_writes) = congested_peer();
        let unreliable: RtcDataChannel = Function::new_no_args("return {readyState:'open', bufferedAmount:0, writes:[], send(data){this.writes.push(Array.from(data));}}")
            .call0(&JsValue::NULL).unwrap().unchecked_into();
        let writes: Array = Reflect::get(unreliable.as_ref(), &"writes".into())
            .unwrap()
            .unchecked_into();
        peer.channels.write().unwrap().insert(method, unreliable);
        let node = NodeId("b".into());
        let peer = Arc::new(peer);
        transport
            .peers
            .write()
            .unwrap()
            .insert(node.clone(), peer.clone());
        transport
            .send_wait(&node, Bytes::from_static(b"unreliable-only"), method)
            .await
            .unwrap();
        assert_eq!(writes.length(), 1);
        assert_eq!(reliable_writes.length(), 0);
        assert_eq!(
            peer.send_queue.lock().unwrap().front().unwrap().as_ref(),
            b"older"
        );
    }
}

#[wasm_bindgen_test]
async fn adaptive_watermark_blocks_bulk_but_control_uses_fixed_limit() {
    use async_trait::async_trait;
    use mistlib_core::{
        action::SendLane,
        config::StorageTransferConfig,
        signaling::{MessageContent, Signaler},
        transport::Transport,
    };
    use mistlib_wasm::transport::webrtc::WasmWebRtcTransport;
    use std::sync::Arc;
    struct NoSignaler;
    #[async_trait(?Send)]
    impl Signaler for NoSignaler {
        async fn send_signaling(
            &self,
            _: &NodeId,
            _: MessageContent,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        async fn close(&self) -> mistlib_core::error::Result<()> {
            Ok(())
        }
    }
    let config = StorageTransferConfig {
        bulk_target_delay_ms: 100,
        control_priority: true,
        unsequenced_bulk: true,
        ..Default::default()
    };
    let transport = WasmWebRtcTransport::new(Arc::new(NoSignaler), NodeId("a".into()));
    transport.set_storage_transfer(config.clone());
    let (peer, dc, writes) = congested_peer();
    peer.send_queue.lock().unwrap().clear();
    peer.configure_bulk_pacing(&config);
    Reflect::set(&dc, &"bufferedAmount".into(), &100000u32.into()).unwrap();
    let node = NodeId("paced-peer".into());
    let peer = Arc::new(peer);
    transport
        .peers
        .write()
        .unwrap()
        .insert(node.clone(), peer.clone());
    let cid = mistlib_core::storage::compute_cid(b"data", 0x55);
    let frame = mistlib_core::overlay::OverlayEnvelope::new(
        NodeId("a".into()),
        node.clone(),
        1,
        MessageContent::Raw(
            mistlib_core::storage::protocol::build_have_chunk_message(&cid, 0, 1, b"data").into(),
        ),
    );
    let bulk = transport.send_in_lane(
        &node,
        mistlib_core::overlay::wire::serialize(&frame)
            .unwrap()
            .into(),
        DeliveryMethod::ReliableOrdered,
        SendLane::Bulk,
        true,
    );
    futures::pin_mut!(bulk);
    assert!(futures::poll!(bulk.as_mut()).is_pending());
    assert_eq!(dc.buffered_amount_low_threshold(), 32768);
    assert_eq!(writes.length(), 0);
    // The bulk writer waits with its own gate even on reliable fallback.
    transport
        .send_in_lane(
            &node,
            Bytes::from_static(b"control"),
            DeliveryMethod::ReliableOrdered,
            SendLane::Control,
            true,
        )
        .await
        .unwrap();
    assert_eq!(writes.length(), 1);
    Reflect::set(&dc, &"bufferedAmount".into(), &0u32.into()).unwrap();
    bulk.await.unwrap();
    assert_eq!(writes.length(), 2);
    // The core responder and adapter share the generation's shrinking budget.
    let budget = transport.bulk_admission_budget(&node).unwrap();
    assert!(budget.try_reserve(1024 * 1024).is_err());
    peer.close_all(&node);
    assert!(budget.is_closed());
}

#[wasm_bindgen_test]
async fn public_reliable_unordered_selects_channel_without_storage_flags_and_falls_back() {
    use async_trait::async_trait;
    use mistlib_core::{
        signaling::{MessageContent, Signaler},
        transport::Transport,
    };
    use mistlib_wasm::transport::webrtc::WasmWebRtcTransport;
    use std::sync::Arc;
    struct NoSignaler;
    #[async_trait(?Send)]
    impl Signaler for NoSignaler {
        async fn send_signaling(
            &self,
            _: &NodeId,
            _: MessageContent,
        ) -> mistlib_core::error::Result<()> {
            Ok(())
        }
        async fn close(&self) -> mistlib_core::error::Result<()> {
            Ok(())
        }
    }
    let transport = WasmWebRtcTransport::new(Arc::new(NoSignaler), NodeId("a".into()));
    transport.set_storage_transfer(mistlib_core::config::StorageTransferConfig {
        unsequenced_bulk: false,
        control_priority: false,
        bulk_channel: false,
        bulk_target_delay_ms: 0,
        ..Default::default()
    });
    let (peer, reliable, reliable_writes) = congested_peer();
    peer.clear_send_queue();
    Reflect::set(reliable.as_ref(), &"bufferedAmount".into(), &0u32.into()).unwrap();
    let unordered: RtcDataChannel = Function::new_no_args("return {label:'reliable-unordered', readyState:'open', bufferedAmount:0, writes:[], send(data){this.writes.push(Array.from(data));}, close(){this.readyState='closed';}}")
        .call0(&JsValue::NULL).unwrap().unchecked_into();
    let unordered_writes: Array = Reflect::get(unordered.as_ref(), &"writes".into())
        .unwrap()
        .unchecked_into();
    peer.unordered_capability.set_local(true);
    *peer.unordered_channel.write().unwrap() = Some(unordered.clone());
    let node = NodeId("b".into());
    let peer = Arc::new(peer);
    transport
        .peers
        .write()
        .unwrap()
        .insert(node.clone(), peer.clone());
    transport.connection_states.write().unwrap().insert(
        node.clone(),
        mistlib_core::types::ConnectionState::Connected,
    );
    let send = || {
        transport.send(
            &node,
            Bytes::from_static(b"app message"),
            DeliveryMethod::ReliableUnordered,
        )
    };
    // Old peer: an unnegotiated optional handle cannot be selected.
    send().await.unwrap();
    assert_eq!(
        (unordered_writes.length(), reliable_writes.length()),
        (0, 1)
    );
    peer.unordered_capability.accept_remote(Some(1));
    Reflect::set(
        unordered.as_ref(),
        &"readyState".into(),
        &"connecting".into(),
    )
    .unwrap();
    send().await.unwrap();
    assert_eq!(
        (unordered_writes.length(), reliable_writes.length()),
        (0, 2)
    );
    Reflect::set(unordered.as_ref(), &"readyState".into(), &"open".into()).unwrap();
    send().await.unwrap();
    assert_eq!(
        (unordered_writes.length(), reliable_writes.length()),
        (1, 2)
    );
    // Code 0 still uses ordered even when the optional channel is open.
    transport
        .send(
            &node,
            Bytes::from_static(b"ordered app"),
            DeliveryMethod::ReliableOrdered,
        )
        .await
        .unwrap();
    assert_eq!(
        (unordered_writes.length(), reliable_writes.length()),
        (1, 3)
    );
    unordered.close();
    send().await.unwrap();
    assert_eq!(
        (unordered_writes.length(), reliable_writes.length()),
        (1, 4)
    );
    peer.unordered_channel.write().unwrap().take();
    send().await.unwrap();
    assert_eq!(
        (unordered_writes.length(), reliable_writes.length()),
        (1, 5)
    );
    // Legacy enqueue-only sends also inherit reliable ordered's reconnect FIFO.
    Reflect::set(
        reliable.as_ref(),
        &"readyState".into(),
        &"connecting".into(),
    )
    .unwrap();
    transport.connection_states.write().unwrap().insert(
        node.clone(),
        mistlib_core::types::ConnectionState::Connecting,
    );
    send().await.unwrap();
    assert!(!peer.send_queue.lock().unwrap().is_empty());
    assert_eq!(reliable_writes.length(), 5);
    Reflect::set(reliable.as_ref(), &"readyState".into(), &"open".into()).unwrap();
    send().await.unwrap();
    assert!(peer.send_queue.lock().unwrap().is_empty());
    assert_eq!(reliable_writes.length(), 7);
    assert!(transport.peers.read().unwrap().contains_key(&node));
}
