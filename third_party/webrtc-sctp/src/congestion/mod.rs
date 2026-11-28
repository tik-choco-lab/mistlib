//! MistLib research patch: selectable sender congestion control (see MISTLIB_PATCHES.md).
//! `Reno` is upstream's RFC 4960 §7.2 code in association_internal.rs, unchanged.
//! `Bbr3` replaces only the controller: the BBRv3 model (`bbr3`), the TSN
//! adapter and the pacer (`sctp`). Loss detection, retransmission and the wire
//! format stay SCTP's. See MISTLIB_PATCHES.md.
pub(crate) mod bbr3;
pub(crate) mod sctp;

/// Sender congestion controller for newly created associations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CongestionControl {
    /// Upstream RFC 4960 slow start / congestion avoidance, without pacing.
    #[default]
    Reno,
    /// Research BBRv3 (draft-ietf-ccwg-bbr-06) with a token-bucket pacer.
    Bbr3,
}

impl CongestionControl {
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Reno => 0,
            Self::Bbr3 => 1,
        }
    }

    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Bbr3,
            _ => Self::Reno,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_round_trips_and_defaults_to_reno() {
        assert_eq!(CongestionControl::default(), CongestionControl::Reno);
        for cc in [CongestionControl::Reno, CongestionControl::Bbr3] {
            assert_eq!(CongestionControl::from_u8(cc.to_u8()), cc);
        }
        assert_eq!(CongestionControl::from_u8(0), CongestionControl::Reno);
        assert_eq!(CongestionControl::from_u8(7), CongestionControl::Reno);
    }
}
