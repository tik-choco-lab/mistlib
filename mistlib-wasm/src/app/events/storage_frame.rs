use mistlib_core::engine::EngineEvent;
use mistlib_core::storage::protocol::{classify_storage_frame, StorageFrame};

/// Storage raw events are consumed before looking up or invoking the JS callback.
pub(super) fn classify_raw_message(event: &EngineEvent) -> Option<StorageFrame<'_>> {
    match event {
        EngineEvent::RawMessage(_, data) => classify_storage_frame(data),
        _ => None,
    }
}
