/// The ordered list of `(media_kind, mid)` pairs describing an SDP's m-line
/// structure, e.g. `[("audio", "0"), ("video", "1")]`. Two SDPs describing
/// the same offer/answer round-trip must produce equal signatures; a
/// mismatch means an answer doesn't correspond to the offer it's being
/// matched against (see `mline_signature`'s doc comment for the field
/// failure this guards).
pub type MlineSignature = Vec<(String, String)>;

/// Extracts `sdp`'s m-line signature: the ordered `(media_kind, mid)` pairs
/// for each `m=` section, in the order they appear. `mid` is the empty
/// string if the section has no `a=mid:` line.
///
/// Kept a plain function over `&str` with no `web_sys` types so it's
/// host-testable via the `#[path]` trick, same as the rest of this
/// directory's pure modules (see `tests/sdp_lines.rs`).
///
/// This is what `SignalingType::Answer`'s handler in `webrtc.rs` uses to
/// guard against a confirmed field failure: a duplicate/late answer for a
/// *previous* local offer arriving after we've already replaced that offer
/// with a newer one (e.g. offer B's answer arriving after we've already sent
/// offer C). Chrome's `set_remote_description` rejects it with "The order of
/// m-lines in answer doesn't match order in offer" because the answer's
/// m-line count/order no longer matches the current local offer -- but the
/// handler used to react to *that* rejection by rolling back to `Stable`,
/// which discards our own still-valid in-flight offer C. When the genuine
/// answer to C then arrives, signaling is already `Stable` instead of
/// `HaveLocalOffer`, so it's rejected too ("Answer precondition failed:
/// signaling state is not HaveLocalOffer") and the negotiation change (e.g.
/// a track publish) is silently lost until some later recovery event
/// happens to renegotiate.
///
/// Comparing m-line signatures *before* calling `set_remote_description`
/// catches this case up front: if the incoming answer's signature doesn't
/// match the local offer's, it can't be the answer to our current offer, so
/// it's ignored instead of being handed to `set_remote_description` (and,
/// critically, instead of rolling back the live offer on the resulting
/// rejection).
pub fn mline_signature(sdp: &str) -> MlineSignature {
    let mut sections: MlineSignature = Vec::new();
    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("m=") {
            let kind = rest.split_whitespace().next().unwrap_or("").to_string();
            sections.push((kind, String::new()));
        } else if let Some(mid) = line.strip_prefix("a=mid:") {
            if let Some(current) = sections.last_mut() {
                current.1 = mid.trim().to_string();
            }
        }
    }
    sections
}
