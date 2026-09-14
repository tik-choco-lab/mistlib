#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferApplyOutcome {
    Applied,
    Rebuild,
}

/// A rejected SDP alone must not destroy a recoverable connection. Match
/// only the observed browser error; unknown errors keep the existing policy.
pub fn should_rebuild_after_offer_failure(error: &str, rollback_stable: bool) -> bool {
    !rollback_stable && error.contains("Failed to set SSL role for the transport")
}
