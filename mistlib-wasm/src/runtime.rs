use async_trait::async_trait;
use mistlib_core::runtime::AsyncRuntime;
use web_time::Duration;
use wasm_bindgen_futures::spawn_local;

pub struct WasmRuntime;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl AsyncRuntime for WasmRuntime {
    fn spawn(&self, future: mistlib_core::runtime::BoxTask) {
        spawn_local(future);
    }

    async fn sleep(&self, duration: Duration) {
        gloo_timers::future::TimeoutFuture::new(duration.as_millis() as u32).await;
    }
}
