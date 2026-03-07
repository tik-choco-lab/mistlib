use crate::types::HostSendSync;
use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;

#[cfg(not(target_arch = "wasm32"))]
pub type BoxTask = Pin<Box<dyn Future<Output = ()> + Send>>;
#[cfg(target_arch = "wasm32")]
pub type BoxTask = Pin<Box<dyn Future<Output = ()>>>;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait AsyncRuntime: HostSendSync {
    fn spawn(&self, future: BoxTask);
    async fn sleep(&self, duration: web_time::Duration);
}
