use async_trait::async_trait;
use mistlib_core::runtime::AsyncRuntime;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::runtime::Handle;

static TASK_COUNTER: AtomicUsize = AtomicUsize::new(0);
const TASK_LOG_THRESHOLD: usize = 200;

pub fn current_task_count() -> usize {
    TASK_COUNTER.load(Ordering::Relaxed)
}

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
        let fut = async move {
            TASK_COUNTER.fetch_add(1, Ordering::Relaxed);
            let res = future.await;
            let count = TASK_COUNTER.fetch_sub(1, Ordering::Relaxed) - 1;
            if count >= TASK_LOG_THRESHOLD {
                tracing::warn!("[tokio] task count high: {}", count);
            }
            res
        };
        self.handle.spawn(Box::pin(fut));
    }

    async fn sleep(&self, duration: web_time::Duration) {
        tokio::time::sleep(duration).await;
    }
}
