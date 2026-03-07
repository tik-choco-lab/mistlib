use async_trait::async_trait;
use mistlib_core::runtime::AsyncRuntime;
use std::future::Future;
use std::pin::Pin;
use tokio::runtime::Handle;

pub struct TokioRuntime {
    pub handle: Handle,
}

impl TokioRuntime {
    pub fn new(handle: Handle) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl AsyncRuntime for TokioRuntime {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        self.handle.spawn(future);
    }

    async fn sleep(&self, duration: web_time::Duration) {
        tokio::time::sleep(duration).await;
    }
}
