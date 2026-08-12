/// Returns the per-peer message limit after applying the SCTP association's
/// negotiated maximum, when the browser exposes one. The negotiated value is
/// only allowed to clamp the configured limit downward: `max_message_bytes`
/// is an intentional application-level ceiling and must not be expanded just
/// because a peer advertises a larger transport capability.
///
/// A missing or zero negotiated value leaves the configured limit unchanged.
/// This preserves the previous behavior in browsers where `pc.sctp` or its
/// `maxMessageSize` property cannot be read.
pub fn effective_message_limit(config_limit: u32, negotiated: Option<u32>) -> u32 {
    match negotiated {
        Some(limit) if limit > 0 => config_limit.min(limit),
        _ => config_limit,
    }
}
