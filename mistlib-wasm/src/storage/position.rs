//! `SelfPositionSource` (SPEC-16) implementation for wasm: reports each
//! active session's own position from its `NodeStore` entry keyed by
//! `crate::app::self_id()`. Storage is process-wide (rule 10) while
//! positions are per-session (SPEC-15 multi-room), so this walks every
//! joined room in join order -- `StorageEngine::resolve_auto_position` takes
//! the first element for auto-tagging, and the eviction/decay paths use the
//! full set for min-distance spatial scoring.

use async_trait::async_trait;
use mistlib_core::storage::SelfPositionSource;
use mistlib_core::types::Vector3;

pub struct WasmSelfPositions;

#[async_trait(?Send)]
impl SelfPositionSource for WasmSelfPositions {
    async fn self_positions(&self) -> Vec<Vector3> {
        let self_id = crate::app::self_id();
        crate::app::all_session_engines()
            .iter()
            .filter_map(|engine| {
                // Locked and read within this synchronous closure -- never
                // held across an `.await` point.
                let store = engine.node_store.lock().unwrap();
                store.nodes.get(&self_id).map(|info| info.position)
            })
            .collect()
    }
}
