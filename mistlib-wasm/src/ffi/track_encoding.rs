pub(super) fn validate_track_encoding(
    max_bitrate_bps: Option<u32>,
    max_framerate: Option<f64>,
    degradation_preference: Option<&str>,
) -> Result<(), &'static str> {
    if max_bitrate_bps == Some(0) {
        return Err("max_bitrate_bps must be greater than zero");
    }
    if max_framerate.is_some_and(|fps| !fps.is_finite() || fps <= 0.0) {
        return Err("max_framerate must be finite and greater than zero");
    }
    if !matches!(
        degradation_preference,
        None | Some("maintain-framerate" | "maintain-resolution" | "balanced")
    ) {
        return Err(
            "degradation_preference must be maintain-framerate, maintain-resolution, or balanced",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_track_encoding;

    #[test]
    fn accepts_clearing_and_valid_overrides() {
        assert_eq!(validate_track_encoding(None, None, None), Ok(()));
        for preference in ["maintain-framerate", "maintain-resolution", "balanced"] {
            assert_eq!(
                validate_track_encoding(Some(8_000_000), Some(29.97), Some(preference)),
                Ok(())
            );
        }
    }

    #[test]
    fn rejects_invalid_overrides() {
        assert!(validate_track_encoding(Some(0), None, None).is_err());
        for fps in [0.0, -0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(validate_track_encoding(None, Some(fps), None).is_err());
        }
        for preference in ["", "Balanced", "disabled", "maintain-resolution "] {
            assert!(validate_track_encoding(None, None, Some(preference)).is_err());
        }
    }
}
