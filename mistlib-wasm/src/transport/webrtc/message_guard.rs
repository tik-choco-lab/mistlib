use mistlib_core::error::MistError;

/// Outcome of a message that passed the size check, distinguishing an
/// unremarkable send from one close enough to `max_message_bytes` to be
/// worth a heads-up log. Decided purely from `size`/`limit` so this stays
/// host-testable via the `#[path]` trick (see
/// `mistlib-wasm/tests/message_guard.rs`), same as `offer_guard`/`ice_config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeCheck {
    Ok,
    /// At or above 80% of `limit`, but not over it.
    NearLimit,
}

/// Validates `size` (a `Transport::send` payload, post-envelope/pre-wire)
/// against `limit` (`config.limits.max_message_bytes`, SPEC-13). Returns
/// `Err(MistError::MessageTooLarge)` if it exceeds the limit, otherwise
/// `Ok(SizeCheck::NearLimit)` at or above 80% of it so the caller can log a
/// warning before congestion turns into an outright rejection.
pub fn check_message_size(size: usize, limit: u32) -> Result<SizeCheck, MistError> {
    if size > limit as usize {
        return Err(MistError::MessageTooLarge { size, limit });
    }
    // size >= 0.8 * limit, written with integer multiplication to avoid
    // floating point and the limit == 0 division-by-zero it would invite.
    if limit > 0 && size as u64 * 5 >= limit as u64 * 4 {
        return Ok(SizeCheck::NearLimit);
    }
    Ok(SizeCheck::Ok)
}
