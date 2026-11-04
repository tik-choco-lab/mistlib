//! Bounded copies at the JS boundary, with real event-loop yields.
use futures::{
    future::{LocalBoxFuture, Shared, WeakShared},
    FutureExt,
};
use js_sys::{Promise, Uint8Array};
use std::{cell::RefCell, future::Future, task::Poll};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::MessageChannel;
use web_time::{Duration, Instant};

const COPY_CHUNK: usize = 256 * 1024;
const COPY_BUDGET: Duration = Duration::from_millis(2);

struct YieldChannel(MessageChannel);

impl Drop for YieldChannel {
    fn drop(&mut self) {
        self.0.port1().set_onmessage(None);
        self.0.port1().close();
        self.0.port2().close();
    }
}

async fn yield_to_browser() -> Result<(), JsValue> {
    let channel = YieldChannel(MessageChannel::new()?);
    let promise = Promise::new(&mut |resolve, reject| {
        channel.0.port1().set_onmessage(Some(&resolve));
        if let Err(error) = channel.0.port2().post_message(&JsValue::NULL) {
            let _ = reject.call1(&JsValue::UNDEFINED, &error);
        }
    });
    // The guard also closes both ports if this future is cancelled.
    JsFuture::from(promise).await?;
    Ok(())
}

/// One budget shared by all block resolutions, including concurrent gets.
/// Check before EVERY poll, not just the first await: FuturesUnordered can start,
/// resume and finish many blocks inside a single executor microtask checkpoint.
type TaskGate = Shared<LocalBoxFuture<'static, ()>>;

pub(super) struct PollBudget {
    state: RefCell<PollBudgetState>,
    budget: Duration,
}

struct PollBudgetState {
    started: Instant,
    generation: u64,
    gate: Option<WeakShared<LocalBoxFuture<'static, ()>>>,
}

impl PollBudget {
    pub(super) fn new(budget: Duration) -> Self {
        Self {
            state: RefCell::new(PollBudgetState {
                started: Instant::now(),
                generation: 0,
                gate: None,
            }),
            budget,
        }
    }

    pub(super) async fn run<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut pause: Option<(u64, TaskGate)> = None;
        futures::future::poll_fn(|cx| loop {
            let mut fresh_task = false;
            if let Some((generation, wait)) = &mut pause {
                if std::pin::Pin::new(wait).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                let mut state = self.state.borrow_mut();
                // Only the first waiter resets the shared budget. Other waiters
                // may be polled much later in this same task and must not each
                // grant another full budget after their gate has already opened.
                if *generation == state.generation {
                    state.generation = state.generation.wrapping_add(1);
                    state.started = Instant::now();
                    state.gate = None;
                    fresh_task = true;
                }
                pause = None;
            }
            let mut state = self.state.borrow_mut();
            if !fresh_task && state.started.elapsed() >= self.budget {
                let generation = state.generation;
                let gate = match state.gate.as_ref().and_then(WeakShared::upgrade) {
                    Some(gate) => gate,
                    None => {
                        let gate = async {
                            if yield_to_browser().await.is_err() {
                                // A channel failure still requires a real task.
                                gloo_timers::future::TimeoutFuture::new(0).await;
                            }
                        }
                        .boxed_local()
                        .shared();
                        // The budget must not keep channels alive when every
                        // waiting resolution has been cancelled.
                        state.gate = gate.downgrade();
                        gate
                    }
                };
                pause = Some((generation, gate));
                continue;
            }
            drop(state);
            return future.as_mut().poll(cx);
        })
        .await
    }
}

pub(super) async fn from_js(snapshot: &Uint8Array) -> Result<Vec<u8>, JsValue> {
    let length = snapshot.length() as usize;
    let mut data = Vec::with_capacity(length);
    let mut started = Instant::now();
    for start in (0..length).step_by(COPY_CHUNK) {
        let end = (start + COPY_CHUNK).min(length);
        data.resize(end, 0);
        snapshot
            .subarray(start as u32, end as u32)
            .copy_to(&mut data[start..end]);
        if end < length && started.elapsed() >= COPY_BUDGET {
            yield_to_browser().await?;
            started = Instant::now();
        }
    }
    Ok(data)
}

pub(super) async fn to_js(data: &[u8]) -> Result<Uint8Array, JsValue> {
    let output = Uint8Array::new_with_length(data.len() as u32);
    let mut started = Instant::now();
    for (index, chunk) in data.chunks(COPY_CHUNK).enumerate() {
        let start = index * COPY_CHUNK;
        let end = start + chunk.len();
        output.subarray(start as u32, end as u32).copy_from(chunk);
        if end < data.len() && started.elapsed() >= COPY_BUDGET {
            yield_to_browser().await?;
            started = Instant::now();
        }
    }
    Ok(output)
}
