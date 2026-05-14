use crate::config::Config;
use crate::overlay::node_store::NodeStore;
use crate::runtime::AsyncRuntime;
use crate::types::NodeId;
use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

mod action;
mod events;
mod lifecycle;
mod network;
mod tick;
mod types;

pub use types::{EngineEvent, EngineEventHandler, EngineState, RunningContext};

use events::DummyEngineEventHandler;

pub struct MistEngine {
    pub self_id: Arc<Mutex<NodeId>>,
    pub config: Arc<Mutex<Config>>,
    pub node_store: Arc<Mutex<NodeStore>>,
    pub state: Arc<Mutex<EngineState>>,
    pub runtime: Arc<dyn AsyncRuntime>,
    pub aoi_nodes: Arc<Mutex<HashSet<NodeId>>>,
    run_generation: AtomicU64,
    // Inner Arc lets us clone the handler out of the Mutex before calling on_event,
    // so the lock is not held during the (potentially slow) handler invocation.
    event_handler: Arc<Mutex<Arc<dyn EngineEventHandler>>>,
}

impl MistEngine {
    pub fn new(runtime: Arc<dyn AsyncRuntime>) -> Arc<Self> {
        Arc::new(Self {
            self_id: Arc::new(Mutex::new(NodeId("local".to_string()))),
            config: Arc::new(Mutex::new(Config::new_default())),
            node_store: Arc::new(Mutex::new(NodeStore::new())),
            state: Arc::new(Mutex::new(EngineState::Idle)),
            runtime,
            aoi_nodes: Arc::new(Mutex::new(HashSet::new())),
            run_generation: AtomicU64::new(0),
            event_handler: Arc::new(Mutex::new(Arc::new(DummyEngineEventHandler))),
        })
    }

    pub fn set_event_handler(&self, handler: Arc<dyn EngineEventHandler>) {
        *self
            .event_handler
            .lock()
            .expect("event_handler lock poisoned") = handler;
    }

    /// Returns the current `RunningContext` if the engine is running, otherwise `None`.
    pub(super) fn running_context(&self) -> Option<Arc<RunningContext>> {
        let state = self.state.lock().expect("state lock poisoned");
        if let EngineState::Running(c) = &*state {
            Some(c.clone())
        } else {
            None
        }
    }
}
