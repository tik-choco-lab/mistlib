use super::*;
use crate::engine::tests::session_registry::{fake_session, REGISTRY_TEST_LOCK};
use crate::engine::ENGINE;
use crate::storage::{self, fetch_block, register_block_provider, BlockProvider, FetchOptions};
use async_trait::async_trait;
use bytes::Bytes;
use mistlib_core::error::Result as CoreResult;
use mistlib_core::storage::cid::{compute_cid, MULTICODEC_DAG_CBOR, MULTICODEC_RAW};
use mistlib_core::storage::protocol::*;
use mistlib_core::storage::BlockStore;
use mistlib_core::transport::{NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod};
use std::sync::Mutex;
use std::time::Duration;

struct RecordingTransport(mpsc::UnboundedSender<(Option<NodeId>, Bytes)>);

#[async_trait]
impl Transport for RecordingTransport {
    async fn start(&self, _: Arc<dyn NetworkEventHandler>) -> CoreResult<()> {
        Ok(())
    }
    async fn send(&self, peer: &NodeId, data: Bytes, _: DeliveryMethod) -> CoreResult<()> {
        self.0.send((Some(peer.clone()), data)).unwrap();
        Ok(())
    }
    async fn send_wait(
        &self,
        peer: &NodeId,
        data: Bytes,
        method: DeliveryMethod,
    ) -> CoreResult<()> {
        self.send(peer, data, method).await
    }
    async fn broadcast(&self, data: Bytes, _: DeliveryMethod) -> CoreResult<()> {
        self.0.send((None, data)).unwrap();
        Ok(())
    }
    fn get_connection_state(&self, _: &NodeId) -> ConnectionState {
        ConnectionState::Connected
    }
    async fn connect(&self, _: &NodeId) -> CoreResult<()> {
        Ok(())
    }
    async fn disconnect(&self, _: &NodeId) -> CoreResult<()> {
        Ok(())
    }
    fn get_connected_nodes(&self) -> Vec<NodeId> {
        vec![peer()]
    }
}

type SentFrames = mpsc::UnboundedReceiver<(Option<NodeId>, Bytes)>;
fn session(room: &str) -> (Arc<SessionCtx>, SentFrames) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut ctx = fake_session(room);
    Arc::get_mut(&mut ctx).unwrap().transport = Arc::new(RecordingTransport(tx));
    (ctx, rx)
}

fn peer() -> NodeId {
    NodeId("block-provider-peer".into())
}

fn deliver(ctx: &Arc<SessionCtx>, from: NodeId, data: Vec<u8>) {
    assert!(ENGINE.handle_storage_message(&data, from, ctx));
}

async fn sent(rx: &mut SentFrames) -> (Option<NodeId>, Bytes) {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

fn opts(peer: Option<NodeId>, max_bytes: usize) -> FetchOptions {
    FetchOptions {
        peer,
        max_bytes,
        timeout: Duration::from_secs(2),
    }
}

struct Provider {
    room: String,
    cid: String,
    data: Option<Vec<u8>>,
    calls: Arc<Mutex<Vec<NodeId>>>,
}

#[async_trait]
impl BlockProvider for Provider {
    async fn get_block(&self, room: &str, from: &NodeId, cid: &str) -> Option<Vec<u8>> {
        self.calls.lock().unwrap().push(from.clone());
        (room == self.room && cid == self.cid && from == &peer())
            .then(|| self.data.clone())
            .flatten()
    }
}

#[tokio::test]
async fn provider_serves_without_storage_then_falls_back_and_unregisters() {
    let _lock = REGISTRY_TEST_LOCK.lock().await;
    // Other tests in this module use the same lock and never initialize STORAGE.
    assert!(storage::STORAGE.get().is_none());
    let (ctx, mut rx) = session("provider-room");
    let (other, mut other_rx) = session("provider-other-room");
    let data = vec![7; HAVE_CHUNK_SIZE + 3];
    let cid = compute_cid(&data, MULTICODEC_RAW);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let register = |data| {
        register_block_provider(Arc::new(Provider {
            room: ctx.room_id.clone(),
            cid: cid.clone(),
            data,
            calls: calls.clone(),
        }))
    };
    let declining = register(None);
    let accepting = register(Some(data.clone()));
    let later_calls = Arc::new(Mutex::new(Vec::new()));
    let later = register_block_provider(Arc::new(Provider {
        room: ctx.room_id.clone(),
        cid: cid.clone(),
        data: Some(vec![99]),
        calls: later_calls.clone(),
    }));

    deliver(&ctx, peer(), build_want_message(&cid));
    let mut actual = Vec::new();
    for index in 0..2 {
        let (to, frame) = sent(&mut rx).await;
        assert_eq!(to, Some(peer()));
        let (got_cid, got_index, total, part) = parse_have_chunk_message(&frame).unwrap();
        assert_eq!((got_cid, got_index, total), (cid.clone(), index, 2));
        actual.extend(part);
    }
    assert_eq!(actual, data);
    deliver(&ctx, peer(), build_query_message(&cid));
    assert_eq!(
        sent(&mut rx).await.1.as_ref(),
        build_have_status_message(&cid)
    );
    assert!(
        later_calls.lock().unwrap().is_empty(),
        "first positive provider wins"
    );
    assert!(calls.lock().unwrap().iter().all(|from| from == &peer()));

    storage::handle_want(other.clone(), peer(), cid.clone()).await;
    storage::handle_query(other, peer(), cid.clone()).await;
    assert!(
        other_rx.try_recv().is_err(),
        "room ACL declines both requests"
    );
    drop(accepting);
    drop(later);
    storage::handle_want(ctx.clone(), peer(), cid.clone()).await;
    storage::handle_query(ctx.clone(), peer(), cid.clone()).await;
    assert!(rx.try_recv().is_err(), "dropped handles cannot serve");

    // Initialize only after demonstrating provider-only operation. Populate the
    // actual default backend and prove a declining provider preserves fallback.
    let cache = tempfile::tempdir().unwrap();
    storage::init_storage(&Default::default(), Some(cache.path().to_path_buf())).await;
    let store = storage::fs::NativeBlockStore::new(cache.path())
        .await
        .unwrap();
    store.store_block(&cid, &data).await.unwrap();
    storage::handle_want(ctx.clone(), peer(), cid.clone()).await;
    for _ in 0..2 {
        sent(&mut rx).await;
    }
    storage::handle_query(ctx.clone(), peer(), cid.clone()).await;
    assert_eq!(
        sent(&mut rx).await.1.as_ref(),
        build_have_status_message(&cid)
    );
    drop(declining);
    let count = calls.lock().unwrap().len();
    storage::handle_query(ctx, peer(), cid).await;
    sent(&mut rx).await;
    assert_eq!(calls.lock().unwrap().len(), count);

    // Fetch while STORAGE is live: receiving a valid block must not write it.
    let (fetch_ctx, mut fetch_rx) = session("provider-fetch-no-cache");
    ENGINE
        .insert_session(fetch_ctx.room_id.clone(), fetch_ctx.clone())
        .await;
    let bytes = b"host-only fetched block";
    let fetch_cid = compute_cid(bytes, MULTICODEC_RAW);
    let fetch = fetch_block(
        &fetch_ctx.room_id,
        &fetch_cid,
        opts(Some(peer()), bytes.len()),
    );
    let response = async {
        sent(&mut fetch_rx).await;
        deliver(&fetch_ctx, peer(), build_have_message(&fetch_cid, bytes));
    };
    let (result, ()) = tokio::join!(fetch, response);
    assert_eq!(result.unwrap(), bytes);
    assert!(storage::STORAGE
        .get()
        .unwrap()
        .get_block(&fetch_cid)
        .await
        .unwrap()
        .is_none());
    ENGINE.remove_session(&fetch_ctx.room_id).await;
}

#[tokio::test]
async fn fetch_pins_peer_and_room_and_verifies_embedded_codecs() {
    let _lock = REGISTRY_TEST_LOCK.lock().await;
    let (ctx, mut rx) = session("fetch-peer-room");
    let (other, mut other_rx) = session("fetch-peer-other-room");
    ENGINE
        .insert_session(ctx.room_id.clone(), ctx.clone())
        .await;
    ENGINE
        .insert_session(other.room_id.clone(), other.clone())
        .await;
    for codec in [MULTICODEC_RAW, MULTICODEC_DAG_CBOR, 0x129] {
        let bytes = b"peer-scoped fetch";
        let cid = compute_cid(bytes, codec);
        let fetch = fetch_block(&ctx.room_id, &cid, opts(Some(peer()), bytes.len()));
        tokio::pin!(fetch);
        let respond = async {
            let (to, frame) = sent(&mut rx).await;
            assert_eq!(to, Some(peer()));
            assert_eq!(frame.as_ref(), build_want_message(&cid));
            deliver(&other, peer(), build_have_message(&cid, bytes));
            deliver(
                &ctx,
                NodeId("wrong-peer".into()),
                build_have_message(&cid, bytes),
            );
        };
        tokio::select! {
            result = &mut fetch => panic!("unauthorized responses completed fetch: {result:?}"),
            () = respond => {}
        }
        assert!(futures_util::poll!(&mut fetch).is_pending());
        // Chunks from separate rooms/peers must not mix either.
        deliver(
            &ctx,
            peer(),
            build_have_chunk_message(&cid, 0, 2, &bytes[..4]),
        );
        deliver(
            &other,
            peer(),
            build_have_chunk_message(&cid, 1, 2, &bytes[4..]),
        );
        assert!(futures_util::poll!(&mut fetch).is_pending());
        deliver(
            &ctx,
            peer(),
            build_have_chunk_message(&cid, 1, 2, &bytes[4..]),
        );
        assert_eq!(fetch.await.unwrap(), bytes);
    }
    assert!(other_rx.try_recv().is_err());
    ENGINE.remove_session(&ctx.room_id).await;
    ENGINE.remove_session(&other.room_id).await;
}

#[tokio::test]
async fn fetch_discovery_uses_only_requested_room_and_ignores_global_peer_cache() {
    let _lock = REGISTRY_TEST_LOCK.lock().await;
    let (ctx, mut rx) = session("fetch-discovery-room");
    let (other, mut other_rx) = session("fetch-discovery-other-room");
    ENGINE
        .insert_session(ctx.room_id.clone(), ctx.clone())
        .await;
    ENGINE
        .insert_session(other.room_id.clone(), other.clone())
        .await;
    let bytes = b"room-discovered block";
    let cid = compute_cid(bytes, MULTICODEC_RAW);
    storage::WANT_REGISTRY.register_peer(&cid, NodeId("cached-outside-room".into()));
    let fetch = fetch_block(&ctx.room_id, &cid, opts(None, bytes.len()));
    tokio::pin!(fetch);
    let outside = async {
        let (to, frame) = sent(&mut rx).await;
        assert_eq!(to, None);
        assert_eq!(frame.as_ref(), build_query_message(&cid));
        deliver(&other, peer(), build_have_status_message(&cid));
    };
    tokio::select! {
        result = &mut fetch => panic!("outside room completed discovery: {result:?}"),
        () = outside => {}
    }
    assert!(futures_util::poll!(&mut fetch).is_pending());
    assert!(
        rx.try_recv().is_err(),
        "outside-room status must not trigger WANT"
    );
    deliver(&ctx, peer(), build_have_status_message(&cid));
    let respond = async {
        let (to, frame) = sent(&mut rx).await;
        assert_eq!(to, Some(peer()));
        assert_eq!(frame.as_ref(), build_want_message(&cid));
        deliver(&ctx, peer(), build_have_message(&cid, bytes));
    };
    let (result, ()) = tokio::join!(fetch, respond);
    assert_eq!(result.unwrap(), bytes);
    assert!(other_rx.try_recv().is_err());
    ENGINE.remove_session(&ctx.room_id).await;
    ENGINE.remove_session(&other.room_id).await;
}

#[tokio::test]
async fn fetch_rejects_oversize_payloads_hash_mismatch_and_cleans_only_its_attempt() {
    let _lock = REGISTRY_TEST_LOCK.lock().await;
    let (ctx, mut rx) = session("fetch-errors-room");
    ENGINE
        .insert_session(ctx.room_id.clone(), ctx.clone())
        .await;
    let bytes = b"payload larger than requested limit";
    let cid = compute_cid(bytes, MULTICODEC_RAW);
    for chunks in [false, true] {
        let other = storage::WANT_REGISTRY.register_attempt(&cid, peer());
        storage::WANT_REGISTRY.track_path(other.id, &ctx.room_id);
        let fetch = fetch_block(&ctx.room_id, &cid, opts(Some(peer()), 4));
        let respond = async {
            sent(&mut rx).await;
            if chunks {
                deliver(&ctx, peer(), build_have_chunk_message(&cid, 0, 2, b"123"));
                deliver(&ctx, peer(), build_have_chunk_message(&cid, 1, 2, b"45"));
            } else {
                deliver(&ctx, peer(), build_have_message(&cid, bytes));
            }
        };
        let (result, ()) = tokio::join!(fetch, respond);
        assert!(result.unwrap_err().to_string().contains("byte limit"));
        // The default resolver's independent waiter survives the host limit.
        assert_eq!(
            other.data.await.unwrap(),
            if chunks { &b"12345"[..] } else { bytes }
        );
    }
    let fetch = fetch_block(&ctx.room_id, &cid, opts(Some(peer()), bytes.len()));
    let respond = async {
        sent(&mut rx).await;
        deliver(&ctx, peer(), build_have_message(&cid, b"corrupt"));
    };
    let (result, ()) = tokio::join!(fetch, respond);
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("CID/hash mismatch"));
    assert!(!storage::WANT_REGISTRY
        .outstanding_senders()
        .contains(&peer()));
    ENGINE.remove_session(&ctx.room_id).await;
}

#[tokio::test]
async fn fetch_timeout_cancellation_and_session_close_clean_registrations() {
    let _lock = REGISTRY_TEST_LOCK.lock().await;
    let (ctx, mut rx) = session("fetch-timeout-room");
    ENGINE
        .insert_session(ctx.room_id.clone(), ctx.clone())
        .await;
    let cid = compute_cid(b"never returned", MULTICODEC_RAW);
    for target in [Some(peer()), None] {
        let result = fetch_block(
            &ctx.room_id,
            &cid,
            FetchOptions {
                peer: target,
                max_bytes: 32,
                timeout: Duration::from_millis(10),
            },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("timeout"));
        sent(&mut rx).await;
        assert!(!storage::WANT_REGISTRY
            .outstanding_senders()
            .contains(&peer()));
    }
    {
        let fetch = fetch_block(&ctx.room_id, &cid, opts(Some(peer()), 32));
        tokio::pin!(fetch);
        assert!(futures_util::poll!(&mut fetch).is_pending());
        sent(&mut rx).await;
        deliver(
            &ctx,
            peer(),
            build_have_chunk_message(&cid, 0, 2, b"partial"),
        );
    }
    assert!(!storage::WANT_REGISTRY
        .outstanding_senders()
        .contains(&peer()));
    ctx.cancel.cancel();
    let result = fetch_block(&ctx.room_id, &cid, opts(Some(peer()), 32)).await;
    assert!(result.unwrap_err().to_string().contains("session closed"));
    ENGINE.remove_session(&ctx.room_id).await;
    assert!(fetch_block(&ctx.room_id, &cid, opts(Some(peer()), 32))
        .await
        .is_err());
}
