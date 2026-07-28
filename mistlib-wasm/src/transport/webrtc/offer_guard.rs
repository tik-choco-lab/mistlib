use mistlib_core::types::ConnectionState;

/// A signaling-state snapshot for the *existing* peer an offer arrived for,
/// boiled down to what `offer_action_for_snapshot` cares about. Kept
/// dependency-free (no `web_sys::RtcSignalingState`) so this stays
/// host-testable via the `#[path]` trick, same as the rest of this module;
/// the wasm-only caller (`WasmWebRtcTransport::handle_offer`) maps
/// `RtcSignalingState` down to this before calling in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalingSnapshot {
    Stable,
    HaveLocalOffer,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferAction {
    IgnoreAtCapacity,
    /// Discard the existing peer connection entirely and build a fresh one for
    /// this offer. Unlike `ApplyInPlace`, this is NOT a renegotiation: the offer
    /// comes from a different session of the remote (it restarted), so the
    /// existing `RTCPeerConnection`'s ICE credentials, DTLS fingerprint and SCTP
    /// association all belong to a peer instance that no longer exists. Applying
    /// such an offer in place forces an implicit ICE+DTLS+SCTP restart on a
    /// connection that was never negotiated with this instance, which fails
    /// unpredictably. The caller must close the old peer and take the
    /// create-from-scratch path.
    ReplacePeer,
    /// Apply the offer in-place to the existing `RTCPeerConnection`
    /// (set_remote_description -> create_answer -> set_local_description)
    /// instead of tearing it down and starting over. Only reachable when a
    /// peer already exists, the remote has not restarted, and its signaling
    /// state is `Stable` -- i.e. this is a genuine renegotiation (new track,
    /// ICE restart, ...), not a collision with our own in-flight offer and
    /// not a stale connection to a peer instance that no longer exists (see
    /// `ReplacePeer`).
    ApplyInPlace,
    /// Perfect-negotiation yield: an inbound offer arrived while *we* have an
    /// offer of our own in flight on this same peer (`HaveLocalOffer`) --
    /// true JSEP glare, e.g. a native peer renegotiating its cascade video at
    /// the same moment we renegotiate a deferred screen-share track. wasm is
    /// unconditionally the polite side (see `offer_action_for_snapshot`'s doc
    /// for why "unconditionally" -- there is no impolite/id-comparison branch
    /// left): rather than protecting our own offer by ignoring theirs (the
    /// old id-compared `IgnoreGlare`, now removed) or leaving their offer to
    /// go unanswered (`DeferTransient`), we abandon our offer and apply
    /// theirs. Handled the same way `ApplyInPlace` is -- `set_remote_description`
    /// with an offer while in `HaveLocalOffer` is a spec-mandated *implicit
    /// rollback* in Chrome, so the exact same
    /// set_remote_description -> create_answer -> set_local_description
    /// sequence works unmodified starting from `HaveLocalOffer` instead of
    /// `Stable`; the caller additionally marks
    /// `Peer::needs_track_reconcile` so whatever our now-abandoned offer was
    /// carrying (a published track, an ICE restart) gets re-proposed once
    /// signaling settles back to `Stable`, instead of silently vanishing.
    ///
    /// This is why a *native* peer can never be stuck in `HaveLocalOffer`
    /// against us: webrtc-rs 0.13 has no rollback transitions at all, so if
    /// wasm were ever the impolite side too, an offer crossing the native
    /// peer's own in-flight offer would deadlock forever (verified in
    /// webrtc-rs source; native can't yield, so wasm must always be willing
    /// to).
    ///
    /// wasm<->wasm pairs: both sides run this same unconditional rule, so a
    /// crossed pair of offers between two wasm peers has BOTH sides yield --
    /// each rolls back its own in-flight offer and answers the other's. That
    /// is not a livelock: it is two independent, well-formed O/A exchanges
    /// (peer A's offer answered by peer B, and peer B's offer answered by
    /// peer A) that each land back on `Stable` on their own PC, just each
    /// carrying only the *other* side's content -- whichever side's own
    /// change (e.g. a new track) was in the abandoned offer will be missing
    /// from its PC's negotiated state afterwards. That's exactly the case
    /// `needs_track_reconcile` exists for: once a settle-point drains it, the
    /// abandoned side re-offers its own change on top of the now-Stable
    /// connection. Worst case is a further collision needing another round;
    /// there is no scenario (unlike the native side) where either wasm peer
    /// is *unable* to ever roll back, so this always makes progress instead
    /// of deadlocking.
    ///
    /// Like `ApplyInPlace`, only reachable when the remote has not restarted:
    /// yielding presumes there is real glare to resolve between our in-flight
    /// offer and theirs on the *same* peer instance. If the remote restarted,
    /// there is no glare at all -- the inbound offer is a brand-new
    /// instance's very first offer, which has never seen ours and isn't
    /// colliding with it, so `ReplacePeer` takes priority instead (see its
    /// doc).
    YieldAndApply,
    /// An existing peer, but neither `Stable` (`ApplyInPlace`) nor our own
    /// offer colliding with theirs (`YieldAndApply`): a transient signaling
    /// state such as `HaveRemoteOffer` (our own answer to *their* prior offer
    /// is still being applied). Do nothing to the live connection -- this
    /// offer simply goes unanswered this round; the remote's own
    /// retry/renegotiation (or, for a genuine ICE restart offer, the next
    /// attempt once signaling settles back to `Stable`) recovers it. Discarding
    /// and recreating the peer here would destroy a healthy connection's
    /// DataChannels/tracks over what is usually a momentary race -- unless the
    /// remote has restarted, in which case there is no healthy connection left
    /// to protect and deferring would only mean answering nothing while
    /// waiting for a retry that hits the same dead peer; `ReplacePeer` takes
    /// priority in that case instead.
    DeferTransient,
    Accept {
        newly_reserved: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferCreateFailureRollback {
    RemoveReservation,
    KeepExistingState,
}

pub fn active_connection_count(states: impl IntoIterator<Item = ConnectionState>) -> usize {
    states
        .into_iter()
        .filter(|state| {
            matches!(
                state,
                ConnectionState::Connected
                    | ConnectionState::Connecting
                    | ConnectionState::Reconnecting
            )
        })
        .count()
}

/// Decides what to do with an inbound offer for an existing (or possibly
/// brand-new) peer. wasm implements perfect negotiation *unconditionally
/// polite*: unlike the textbook version (where politeness is assigned by
/// comparing peer ids, so exactly one side of any given pair yields), there
/// is no impolite branch here at all -- every wasm peer always yields its own
/// in-flight offer to an inbound one. This is required, not just simpler:
/// `mistlib-native`'s webrtc-rs 0.13 has no rollback transitions, so a native
/// peer can *never* be the polite side; if wasm were sometimes impolite too,
/// a crossed offer against a native peer would deadlock forever (neither
/// side able to yield). Making wasm always polite means the native peer,
/// which by construction only ever needs to be the impolite side, always has
/// a partner willing to roll back -- see `OfferAction::YieldAndApply`'s doc
/// for the wasm<->wasm convergence argument this implies.
pub fn offer_action_for_snapshot(
    peer_exists: bool,
    remote_restarted: bool,
    state: Option<ConnectionState>,
    active_connections: usize,
    max_connections: usize,
    signaling_state: SignalingSnapshot,
) -> OfferAction {
    if peer_exists {
        // A restarted remote outranks every other check below, including
        // `DeferTransient` and `YieldAndApply`: the existing `RTCPeerConnection`
        // belongs to a peer instance that no longer exists, so its ICE
        // credentials, DTLS fingerprint and SCTP association are all dead.
        // Deferring would just mean answering nothing while waiting for a
        // retry that hits the same dead peer, and there is no glare to yield
        // on -- the remote's offer has never seen ours, because it's coming
        // from a brand-new instance. See `OfferAction::ReplacePeer`.
        if remote_restarted {
            return OfferAction::ReplacePeer;
        }
        // A Stable existing peer means this offer is a genuine renegotiation
        // (e.g. the remote added a track, or is doing an ICE restart) --
        // apply it in-place and keep the existing DataChannels/tracks alive,
        // rather than discarding a healthy connection.
        if signaling_state == SignalingSnapshot::Stable {
            return OfferAction::ApplyInPlace;
        }
        // Our own offer is in flight on this same peer connection: true JSEP
        // glare. Unconditionally yield -- see `OfferAction::YieldAndApply`.
        if signaling_state == SignalingSnapshot::HaveLocalOffer {
            return OfferAction::YieldAndApply;
        }
        // Any other state for an existing peer (HaveRemoteOffer, ...) is a
        // transient in-between, not a case where starting over is safe --
        // see `OfferAction::DeferTransient`. Note this means an existing peer
        // never reaches the create-from-scratch `Accept` path below; that
        // path is only for brand-new connections. (A restarted remote instead
        // takes the `ReplacePeer` path above, which is its own from-scratch
        // signal distinct from `Accept` -- the caller closes the old peer and
        // creates a new one, rather than this function ever returning `Accept`
        // for a `peer_exists = true` snapshot.)
        return OfferAction::DeferTransient;
    }

    if state.is_none() {
        if active_connections >= max_connections {
            return OfferAction::IgnoreAtCapacity;
        }
        return OfferAction::Accept {
            newly_reserved: true,
        };
    }

    OfferAction::Accept {
        newly_reserved: false,
    }
}

pub fn create_failure_rollback(newly_reserved: bool) -> OfferCreateFailureRollback {
    if newly_reserved {
        OfferCreateFailureRollback::RemoveReservation
    } else {
        OfferCreateFailureRollback::KeepExistingState
    }
}
