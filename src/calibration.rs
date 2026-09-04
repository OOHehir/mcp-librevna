//! Tracking whether the active calibration still applies to the current sweep.
//!
//! This is the quietest hazard in the whole server. Changing the span or point
//! count after calibrating does not fail, and does not look wrong: the
//! instrument keeps returning smooth, plausible traces. But those traces are
//! interpolated from, or extrapolated beyond, the frequencies that were
//! actually measured during calibration. An agent reporting them as calibrated
//! results would be confidently wrong.
//!
//! So the sweep configuration is recorded when a calibration is applied, and
//! every measurement result carries the verdict below.

use schemars::JsonSchema;
use serde::Serialize;

/// The sweep settings a measurement or calibration was taken at.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
pub struct SweepConfig {
    pub start_hz: f64,
    pub stop_hz: f64,
    pub points: usize,
}

impl SweepConfig {
    pub fn new(start_hz: f64, stop_hz: f64, points: usize) -> Self {
        Self {
            start_hz,
            stop_hz,
            points,
        }
    }

    /// Whether this sweep lies entirely within `other`'s frequency range.
    ///
    /// A small tolerance absorbs the float round-tripping through SCPI text,
    /// so a span that was set and read back unchanged does not read as a
    /// fractional-hertz excursion.
    fn within(&self, other: &SweepConfig) -> bool {
        const TOLERANCE_HZ: f64 = 1.0;
        self.start_hz >= other.start_hz - TOLERANCE_HZ
            && self.stop_hz <= other.stop_hz + TOLERANCE_HZ
    }
}

/// How much the active calibration can be trusted for the current sweep.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CalValidity {
    /// No calibration is active. Readings are raw.
    None,
    /// The sweep matches the calibration exactly.
    Valid,
    /// Within the calibrated range, but at points that were not measured.
    Interpolated { reason: String },
    /// The sweep has moved outside the calibrated range entirely.
    Invalid { reason: String },
}

impl CalValidity {
    /// Whether results under this verdict may be described as calibrated.
    pub fn is_trustworthy(&self) -> bool {
        matches!(self, CalValidity::Valid)
    }

    /// A warning to attach to a measurement, if one is warranted.
    pub fn warning(&self) -> Option<String> {
        match self {
            CalValidity::Valid => None,
            CalValidity::None => {
                Some("No calibration is active: these are raw, uncorrected readings.".into())
            }
            CalValidity::Interpolated { reason } => Some(format!(
                "Calibration is being interpolated: {reason} \
                 Values are approximate; re-calibrate at this sweep for accurate results."
            )),
            CalValidity::Invalid { reason } => Some(format!(
                "Calibration does not cover this sweep: {reason} \
                 These readings are effectively uncalibrated."
            )),
        }
    }
}

/// The calibration currently loaded on the instrument, and where it came from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, JsonSchema)]
pub struct CalibrationState {
    /// The calibration type reported by the instrument, e.g. `SOLT`.
    pub active_type: Option<String>,
    /// The sweep the calibration was performed at.
    pub taken_at: Option<SweepConfig>,
}

impl CalibrationState {
    /// Record that a calibration became active at the given sweep.
    pub fn applied(&mut self, cal_type: impl Into<String>, sweep: SweepConfig) {
        self.active_type = Some(cal_type.into());
        self.taken_at = Some(sweep);
    }

    /// Record that the calibration was cleared.
    pub fn cleared(&mut self) {
        self.active_type = None;
        self.taken_at = None;
    }

    /// Judge the active calibration against the sweep now configured.
    pub fn validity(&self, current: &SweepConfig) -> CalValidity {
        let (Some(_), Some(taken_at)) = (&self.active_type, &self.taken_at) else {
            return CalValidity::None;
        };

        if !current.within(taken_at) {
            return CalValidity::Invalid {
                reason: format!(
                    "the sweep now spans {:.6} to {:.6} MHz, outside the calibrated \
                     {:.6} to {:.6} MHz.",
                    current.start_hz / 1e6,
                    current.stop_hz / 1e6,
                    taken_at.start_hz / 1e6,
                    taken_at.stop_hz / 1e6,
                ),
            };
        }

        if current.points != taken_at.points {
            return CalValidity::Interpolated {
                reason: format!(
                    "calibrated at {} points, now sweeping {}.",
                    taken_at.points, current.points
                ),
            };
        }

        // Same point count inside a narrower window still lands on frequencies
        // that were never actually measured.
        let same_range = (current.start_hz - taken_at.start_hz).abs() < 1.0
            && (current.stop_hz - taken_at.stop_hz).abs() < 1.0;
        if !same_range {
            return CalValidity::Interpolated {
                reason: format!(
                    "calibrated across {:.6} to {:.6} MHz, now sweeping {:.6} to {:.6} MHz.",
                    taken_at.start_hz / 1e6,
                    taken_at.stop_hz / 1e6,
                    current.start_hz / 1e6,
                    current.stop_hz / 1e6,
                ),
            };
        }

        CalValidity::Valid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calibrated_at(start: f64, stop: f64, points: usize) -> CalibrationState {
        let mut state = CalibrationState::default();
        state.applied("SOLT", SweepConfig::new(start, stop, points));
        state
    }

    #[test]
    fn no_calibration_reports_none_and_warns() {
        let state = CalibrationState::default();
        let verdict = state.validity(&SweepConfig::new(1e6, 6e9, 501));
        assert_eq!(verdict, CalValidity::None);
        assert!(!verdict.is_trustworthy());
        assert!(verdict.warning().unwrap().contains("raw"));
    }

    #[test]
    fn an_unchanged_sweep_is_valid_and_silent() {
        let state = calibrated_at(1e9, 2e9, 501);
        let verdict = state.validity(&SweepConfig::new(1e9, 2e9, 501));
        assert_eq!(verdict, CalValidity::Valid);
        assert!(verdict.is_trustworthy());
        assert!(verdict.warning().is_none());
    }

    #[test]
    fn changing_the_point_count_interpolates() {
        let state = calibrated_at(1e9, 2e9, 501);
        let verdict = state.validity(&SweepConfig::new(1e9, 2e9, 1001));
        assert!(matches!(verdict, CalValidity::Interpolated { .. }));
        assert!(!verdict.is_trustworthy());
        let warning = verdict.warning().unwrap();
        assert!(warning.contains("501"), "{warning}");
        assert!(warning.contains("1001"), "{warning}");
    }

    #[test]
    fn narrowing_the_span_interpolates_even_at_the_same_point_count() {
        // The points are inside the calibrated range, but land at frequencies
        // that were never measured.
        let state = calibrated_at(1e9, 2e9, 501);
        let verdict = state.validity(&SweepConfig::new(1.2e9, 1.8e9, 501));
        assert!(
            matches!(verdict, CalValidity::Interpolated { .. }),
            "{verdict:?}"
        );
    }

    #[test]
    fn widening_the_span_invalidates() {
        let state = calibrated_at(1e9, 2e9, 501);
        for (start, stop) in [(0.5e9, 2.0e9), (1.0e9, 3.0e9), (0.1e9, 6.0e9)] {
            let verdict = state.validity(&SweepConfig::new(start, stop, 501));
            assert!(
                matches!(verdict, CalValidity::Invalid { .. }),
                "{start} to {stop} should be invalid, got {verdict:?}"
            );
            assert!(!verdict.is_trustworthy());
        }
    }

    #[test]
    fn an_invalid_verdict_says_it_is_effectively_uncalibrated() {
        let state = calibrated_at(1e9, 2e9, 501);
        let warning = state
            .validity(&SweepConfig::new(1e6, 6e9, 501))
            .warning()
            .unwrap();
        assert!(warning.contains("uncalibrated"), "{warning}");
    }

    #[test]
    fn sub_hertz_float_drift_does_not_invalidate() {
        // Frequencies round-trip through SCPI as decimal text; a calibration
        // must not be discarded over the resulting fractional hertz.
        let state = calibrated_at(1e9, 2e9, 501);
        let verdict = state.validity(&SweepConfig::new(1e9 - 0.4, 2e9 + 0.4, 501));
        assert_eq!(verdict, CalValidity::Valid, "got {verdict:?}");
    }

    #[test]
    fn clearing_a_calibration_returns_to_none() {
        let mut state = calibrated_at(1e9, 2e9, 501);
        assert!(
            state
                .validity(&SweepConfig::new(1e9, 2e9, 501))
                .is_trustworthy()
        );
        state.cleared();
        assert_eq!(
            state.validity(&SweepConfig::new(1e9, 2e9, 501)),
            CalValidity::None
        );
    }
}
