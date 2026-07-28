pub(super) async fn add(name: &str, data: &[u8]) -> mistlib_core::error::Result<String> {
    if let Some(storage) = crate::storage::STORAGE.get() {
        use mistlib_core::layers::L2Storage;
        storage.add(name, data).await
    } else {
        Err(mistlib_core::error::MistError::Internal(
            "Storage not initialized".to_string(),
        ))
    }
}

/// Explicit-position variant of `add` (SPEC-16): not part of the `L2Storage`
/// core trait, so this calls `P2PStorage::add_at` directly rather than going
/// through it.
pub(super) async fn add_at(
    name: &str,
    data: &[u8],
    position: Option<mistlib_core::types::Vector3>,
) -> mistlib_core::error::Result<String> {
    if let Some(storage) = crate::storage::STORAGE.get() {
        storage.add_at(name, data, position).await
    } else {
        Err(mistlib_core::error::MistError::Internal(
            "Storage not initialized".to_string(),
        ))
    }
}

pub(super) async fn get(cid: &str) -> mistlib_core::error::Result<Vec<u8>> {
    if let Some(storage) = crate::storage::STORAGE.get() {
        use mistlib_core::layers::L2Storage;
        storage.get(cid).await
    } else {
        Err(mistlib_core::error::MistError::Internal(
            "Storage not initialized".to_string(),
        ))
    }
}
