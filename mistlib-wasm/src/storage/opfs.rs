use async_trait::async_trait;
use js_sys::{Object, Promise, Reflect, Uint8Array};
use mistlib_core::error::{MistError, Result};
use mistlib_core::storage::BlockStore;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::FileSystemDirectoryHandle;

const STORAGE_DIR: &str = "mistlib-blocks";

fn make_opts(create: bool) -> Object {
    let obj = Object::new();
    Reflect::set(
        &obj,
        &JsValue::from_str("create"),
        &JsValue::from_bool(create),
    )
    .ok();
    obj
}

pub(crate) async fn call_async_method(
    this: &JsValue,
    method: &str,
    args: &[JsValue],
) -> std::result::Result<JsValue, JsValue> {
    let f = Reflect::get(this, &JsValue::from_str(method))?.dyn_into::<js_sys::Function>()?;
    let arr = js_sys::Array::new();
    for a in args {
        arr.push(a);
    }
    let promise = f.apply(this, &arr)?.dyn_into::<Promise>()?;
    JsFuture::from(promise).await
}

/// Opens (creating if needed) a top-level OPFS directory. Shared by the
/// block store (`mistlib-blocks`) and the meta store (`mistlib-meta`).
pub(crate) async fn get_dir(name: &str) -> Result<FileSystemDirectoryHandle> {
    let window = web_sys::window().ok_or_else(|| MistError::Internal("No window".into()))?;

    let root_js = JsFuture::from(window.navigator().storage().get_directory())
        .await
        .map_err(|e| MistError::Internal(format!("OPFS root: {:?}", e)))?;
    let root: FileSystemDirectoryHandle = root_js
        .dyn_into()
        .map_err(|_| MistError::Internal("root is not FileSystemDirectoryHandle".into()))?;

    let opts = make_opts(true);
    let root_val: &JsValue = root.as_ref();
    let dir_js = call_async_method(
        root_val,
        "getDirectoryHandle",
        &[JsValue::from_str(name), opts.into()],
    )
    .await
    .map_err(|e| MistError::Internal(format!("getDirectoryHandle: {:?}", e)))?;

    dir_js
        .dyn_into::<FileSystemDirectoryHandle>()
        .map_err(|_| MistError::Internal("dir is not FileSystemDirectoryHandle".into()))
}

async fn get_block_dir() -> Result<FileSystemDirectoryHandle> {
    get_dir(STORAGE_DIR).await
}

/// Writes (create-or-truncate) `name` inside `dir`.
pub(crate) async fn write_file(
    dir: &FileSystemDirectoryHandle,
    name: &str,
    data: &[u8],
) -> Result<()> {
    let fh = get_file_handle(dir, name, true)
        .await
        .map_err(|e| MistError::Internal(format!("getFileHandle '{}': {:?}", name, e)))?;

    let fh_val: &JsValue = fh.as_ref();
    let writable_js = call_async_method(fh_val, "createWritable", &[])
        .await
        .map_err(|e| MistError::Internal(format!("createWritable '{}': {:?}", name, e)))?;
    let writable: web_sys::FileSystemWritableFileStream = writable_js
        .dyn_into()
        .map_err(|_| MistError::Internal("not a WritableStream".into()))?;

    let arr = Uint8Array::from(data);
    let w_val: &JsValue = writable.as_ref();
    call_async_method(w_val, "write", &[arr.into()])
        .await
        .map_err(|e| MistError::Internal(format!("write '{}': {:?}", name, e)))?;

    call_async_method(w_val, "close", &[])
        .await
        .map_err(|e| MistError::Internal(format!("close '{}': {:?}", name, e)))?;

    Ok(())
}

/// Reads `name` inside `dir`; `None` if the file doesn't exist.
pub(crate) async fn read_file(
    dir: &FileSystemDirectoryHandle,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    let fh = match get_file_handle(dir, name, false).await {
        Ok(fh) => fh,
        Err(_) => return Ok(None),
    };

    let fh_val: &JsValue = fh.as_ref();
    let file_js = call_async_method(fh_val, "getFile", &[])
        .await
        .map_err(|e| MistError::Internal(format!("getFile '{}': {:?}", name, e)))?;

    let buffer = call_async_method(&file_js, "arrayBuffer", &[])
        .await
        .map_err(|e| MistError::Internal(format!("arrayBuffer '{}': {:?}", name, e)))?;

    Ok(Some(Uint8Array::new(&buffer).to_vec()))
}

/// Removes `name` inside `dir`; missing files are a no-op success.
pub(crate) async fn remove_file(dir: &FileSystemDirectoryHandle, name: &str) -> Result<()> {
    let dir_val: &JsValue = dir.as_ref();
    let _ = call_async_method(dir_val, "removeEntry", &[JsValue::from_str(name)]).await;
    Ok(())
}

async fn get_file_handle(
    dir: &FileSystemDirectoryHandle,
    name: &str,
    create: bool,
) -> std::result::Result<web_sys::FileSystemFileHandle, JsValue> {
    let opts = make_opts(create);
    let dir_val: &JsValue = dir.as_ref();
    let fh_js = call_async_method(
        dir_val,
        "getFileHandle",
        &[JsValue::from_str(name), opts.into()],
    )
    .await?;
    fh_js
        .dyn_into::<web_sys::FileSystemFileHandle>()
        .map_err(|v| v.into())
}

pub struct WasmBlockStore;

#[async_trait(?Send)]
impl BlockStore for WasmBlockStore {
    async fn store_block(&self, cid: &str, data: &[u8]) -> Result<()> {
        let dir = get_block_dir().await?;
        write_file(&dir, cid, data).await
    }

    async fn load_block(&self, cid: &str) -> Result<Option<Vec<u8>>> {
        let dir = get_block_dir().await?;
        read_file(&dir, cid).await
    }

    async fn delete_block(&self, cid: &str) -> Result<()> {
        let dir = get_block_dir().await?;
        remove_file(&dir, cid).await
    }
}
