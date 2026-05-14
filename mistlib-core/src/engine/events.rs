use super::{EngineEvent, EngineEventHandler, MistEngine};

pub(super) struct DummyEngineEventHandler;

impl EngineEventHandler for DummyEngineEventHandler {
    fn on_event(&self, _event: EngineEvent) {}
}

impl MistEngine {
    /// Clones the event handler out of its Mutex and fires an event,
    /// ensuring the lock is not held during the handler invocation.
    pub(super) fn emit(&self, event: EngineEvent) {
        let handler = self
            .event_handler
            .lock()
            .expect("event_handler lock poisoned")
            .clone();
        handler.on_event(event);
    }
}
