//! App-facing KV API (SPEC-17): `storage_kv_set` / `storage_kv_get` /
//! `storage_kv_delete`, backed by `WasmMetaStore` (OPFS, `mistlib-meta`
//! directory).
//!
//! This is a purely local, mutable layer -- **not** part of the
//! content-addressed, P2P-replicated block store. Values written here stay
//! on this device/origin only; nothing here is chunked, gossiped, or synced
//! to peers. Apps that want a value to be shared across peers should put the
//! payload in the CAS via `storage_add` and keep only its CID in the KV.
//!
//! `WasmMetaStore` is stateless, so these functions build one directly
//! instead of going through the `STORAGE` (`P2PStorage`) global in
//! `storage.rs`. That means the KV API works even before `init_storage` is
//! called -- it has no dependency on room/session state.

use js_sys::Uint8Array;
use mistlib_core::storage::meta::{validate_meta_key, validate_meta_value};
use mistlib_core::storage::MetaStore;
use wasm_bindgen::prelude::*;

use crate::storage::meta::WasmMetaStore;

#[wasm_bindgen]
pub async fn storage_kv_set(key: String, data: Uint8Array) -> Result<(), JsValue> {
    validate_meta_key(&key).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let data = data.to_vec();
    validate_meta_value(data.len()).map_err(|e| JsValue::from_str(&e.to_string()))?;

    WasmMetaStore
        .set(&key, &data)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

#[wasm_bindgen]
pub async fn storage_kv_get(key: String) -> Result<Option<Uint8Array>, JsValue> {
    validate_meta_key(&key).map_err(|e| JsValue::from_str(&e.to_string()))?;

    let data = WasmMetaStore
        .get(&key)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    Ok(data.map(|d| Uint8Array::from(d.as_slice())))
}

#[wasm_bindgen]
pub async fn storage_kv_delete(key: String) -> Result<(), JsValue> {
    validate_meta_key(&key).map_err(|e| JsValue::from_str(&e.to_string()))?;

    WasmMetaStore
        .delete(&key)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))
}
