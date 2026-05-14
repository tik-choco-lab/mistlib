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
