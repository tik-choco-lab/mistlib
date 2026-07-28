//! End-to-end test for the `storage_add` / `storage_add_at` / `storage_get`
//! FFI exports.
//!
//! Sets up a real `P2PStorage` backed by a temp-dir block store (the peer
//! resolver/transport is never exercised because add/get hit the local store)
//! and drives the C ABI functions exactly as a Native/Unity host would.
//!
//! Uses plain `#[test]`: the FFI functions internally call
//! `ENGINE.runtime.block_on`, which would panic if invoked from within a Tokio
//! runtime context. Setup that needs async is run on a throwaway runtime.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use mistlib_core::error::Result as CoreResult;
use mistlib_core::storage::{P2PStorage, SpatialPolicy};
use mistlib_core::transport::{NetworkEventHandler, Transport};
use mistlib_core::types::{ConnectionState, DeliveryMethod, NodeId};

use mistlib::storage::fs::NativeBlockStore;
use mistlib::storage::resolver::{FixedTransportSource, NativePeerResolver, WantRegistry};

/// Transport stub: storage add/get against the local block store never calls it.
struct NoopTransport;

#[async_trait]
impl Transport for NoopTransport {
    async fn start(&self, _handler: Arc<dyn NetworkEventHandler>) -> CoreResult<()> {
        Ok(())
    }
    async fn send(&self, _node: &NodeId, _data: Bytes, _method: DeliveryMethod) -> CoreResult<()> {
        Ok(())
    }
    async fn broadcast(&self, _data: Bytes, _method: DeliveryMethod) -> CoreResult<()> {
        Ok(())
    }
    fn get_connection_state(&self, _node: &NodeId) -> ConnectionState {
        ConnectionState::Disconnected
    }
    async fn connect(&self, _node: &NodeId) -> CoreResult<()> {
        Ok(())
    }
    async fn disconnect(&self, _node: &NodeId) -> CoreResult<()> {
        Ok(())
    }
    fn get_connected_nodes(&self) -> Vec<NodeId> {
        vec![]
    }
}

/// Initializes the global STORAGE the FFI functions read from (idempotent).
fn ensure_storage() {
    if mistlib::storage::STORAGE.get().is_some() {
        return;
    }
    let rt = tokio::runtime::Runtime::new().expect("setup runtime");
    let dir = std::env::temp_dir().join(format!("mistlib_ffi_test_{}", std::process::id()));
    let store = rt
        .block_on(NativeBlockStore::new(&dir))
        .expect("block store init");
    let resolver = NativePeerResolver::new(
        Arc::new(FixedTransportSource(vec![Arc::new(NoopTransport)])),
        WantRegistry::new(),
        5000,
    );
    // No `SelfPositionSource` wired up here: add/get against the local store
    // never touches spatial eviction, so `None`/default policy keeps this a
    // pure-LRU setup exactly like before SPEC-16.
    let storage = P2PStorage::new(
        store,
        resolver,
        64 * 1024 * 1024,
        None,
        SpatialPolicy::default(),
    );
    mistlib::storage::STORAGE
        .set(Arc::new(storage))
        .unwrap_or(());
}

#[test]
fn ffi_storage_add_get_round_trip() {
    ensure_storage();

    let payload = b"hello mistlib native storage".to_vec();
    let name = "greeting.txt";

    // --- storage_add ---
    let mut cid_buf = vec![0u8; 256];
    let cid_len = unsafe {
        mistlib::ffi::storage_add(
            name.as_ptr(),
            name.len(),
            payload.as_ptr(),
            payload.len(),
            cid_buf.as_mut_ptr(),
            cid_buf.len(),
        )
    };
    assert!(cid_len > 0, "storage_add should return a non-empty CID");
    let cid = String::from_utf8(cid_buf[..cid_len as usize].to_vec()).expect("CID is utf8");

    // --- storage_get: size query with a zero-length buffer ---
    let needed =
        unsafe { mistlib::ffi::storage_get(cid.as_ptr(), cid.len(), std::ptr::null_mut(), 0) };
    assert_eq!(
        needed as usize,
        payload.len(),
        "size query reports full length"
    );

    // --- storage_get: actual read ---
    let mut out = vec![0u8; needed as usize];
    let read_len =
        unsafe { mistlib::ffi::storage_get(cid.as_ptr(), cid.len(), out.as_mut_ptr(), out.len()) };
    assert_eq!(read_len as usize, payload.len());
    assert_eq!(&out[..read_len as usize], payload.as_slice());
}

#[test]
fn ffi_storage_add_at_round_trip() {
    ensure_storage();

    let payload = b"hello mistlib native storage, spatially tagged".to_vec();
    let name = "greeting_at.txt";

    // --- storage_add_at: explicit position instead of auto-tagging ---
    let mut cid_buf = vec![0u8; 256];
    let cid_len = unsafe {
        mistlib::ffi::storage_add_at(
            name.as_ptr(),
            name.len(),
            payload.as_ptr(),
            payload.len(),
            1.0,
            2.0,
            3.0,
            cid_buf.as_mut_ptr(),
            cid_buf.len(),
        )
    };
    assert!(cid_len > 0, "storage_add_at should return a non-empty CID");
    let cid = String::from_utf8(cid_buf[..cid_len as usize].to_vec()).expect("CID is utf8");

    // --- storage_get: the block is retrievable exactly like a plain add ---
    let needed =
        unsafe { mistlib::ffi::storage_get(cid.as_ptr(), cid.len(), std::ptr::null_mut(), 0) };
    assert_eq!(
        needed as usize,
        payload.len(),
        "size query reports full length"
    );

    let mut out = vec![0u8; needed as usize];
    let read_len =
        unsafe { mistlib::ffi::storage_get(cid.as_ptr(), cid.len(), out.as_mut_ptr(), out.len()) };
    assert_eq!(read_len as usize, payload.len());
    assert_eq!(&out[..read_len as usize], payload.as_slice());
}

#[test]
fn ffi_storage_get_unknown_cid_returns_zero() {
    ensure_storage();

    let cid = "bafy-nonexistent-cid";
    let mut out = vec![0u8; 64];
    let read_len =
        unsafe { mistlib::ffi::storage_get(cid.as_ptr(), cid.len(), out.as_mut_ptr(), out.len()) };
    assert_eq!(read_len, 0, "unknown CID should return 0");
}
