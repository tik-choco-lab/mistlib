use super::WasmNostrSignaler;
use futures::future::{select, Either};
use gloo_timers::future::TimeoutFuture;
use mistlib_core::signaling::nostr::{normalize_relays, parse_relay_list_json};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

const FETCH_TIMEOUT_MS: u32 = 10_000;
const MAX_RELAY_LIST_BYTES: usize = 256 * 1024;

impl WasmNostrSignaler {
    pub(super) async fn resolve_relays(&self) -> Result<Vec<String>, JsValue> {
        let mut relays = self.relays.clone();
        if let Some(url) = &self.relay_list_url {
            match fetch_relay_list(url).await {
                Ok(json) => match parse_relay_list_json(&json) {
                    Ok(list) => relays.extend(list),
                    Err(err) if relays.is_empty() => {
                        return Err(JsValue::from_str(&err.to_string()))
                    }
                    Err(err) => web_sys::console::warn_1(&err.to_string().into()),
                },
                Err(err) if relays.is_empty() => return Err(err),
                Err(err) => web_sys::console::warn_1(&err),
            }
        }
        normalize_relays(relays).map_err(|err| JsValue::from_str(&err.to_string()))
    }
}

async fn fetch_relay_list(url: &str) -> Result<String, JsValue> {
    match select(
        Box::pin(fetch_relay_list_inner(url)),
        Box::pin(TimeoutFuture::new(FETCH_TIMEOUT_MS)),
    )
    .await
    {
        Either::Left((result, _)) => result,
        Either::Right((_, _)) => Err(JsValue::from_str(
            "WasmNostrSignaler: relay list fetch timed out",
        )),
    }
}

async fn fetch_relay_list_inner(url: &str) -> Result<String, JsValue> {
    let window = web_sys::window()
        .ok_or_else(|| JsValue::from_str("WasmNostrSignaler: window is unavailable"))?;
    let response = JsFuture::from(window.fetch_with_str(url)).await?;
    let response: web_sys::Response = response.dyn_into()?;
    if !response.ok() {
        return Err(JsValue::from_str(&format!(
            "WasmNostrSignaler: relay list fetch failed with status {}",
            response.status()
        )));
    }
    let text = JsFuture::from(response.text()?).await?;
    let text = text
        .as_string()
        .ok_or_else(|| JsValue::from_str("WasmNostrSignaler: relay list response is not text"))?;
    if text.len() > MAX_RELAY_LIST_BYTES {
        return Err(JsValue::from_str(
            "WasmNostrSignaler: relay list response is too large",
        ));
    }
    Ok(text)
}
