//! Device identity, capability limits and connection state.

use schemars::JsonSchema;
use serde::Serialize;

use crate::error::{Result, SafetyError};
use crate::scpi::{ScpiClient, parse};

/// The operating modes the instrument can be switched between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum Mode {
    /// Vector network analyser.
    Vna,
    /// Spectrum analyser.
    Sa,
    /// Signal generator.
    Gen,
}

impl Mode {
    pub fn as_scpi(self) -> &'static str {
        match self {
            Mode::Vna => "VNA",
            Mode::Sa => "SA",
            Mode::Gen => "GEN",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "VNA" => Some(Mode::Vna),
            "SA" => Some(Mode::Sa),
            "GEN" => Some(Mode::Gen),
            _ => None,
        }
    }
}

/// The operating envelope this particular unit reports for itself.
///
/// Queried from the instrument rather than hard-coded, so hardware revisions
/// and firmware changes are picked up automatically instead of silently
/// disagreeing with a constant compiled in here.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
pub struct DeviceLimits {
    pub min_freq_hz: f64,
    pub max_freq_hz: f64,
    pub min_ifbw_hz: f64,
    pub max_ifbw_hz: f64,
    pub max_points: usize,
    pub min_power_dbm: f64,
    pub max_power_dbm: f64,
    pub min_rbw_hz: f64,
    pub max_rbw_hz: f64,
}

impl DeviceLimits {
    /// Read the limits from a connected instrument.
    pub async fn query(client: &mut ScpiClient) -> Result<Self> {
        async fn num(client: &mut ScpiClient, cmd: &str) -> Result<f64> {
            let raw = client.query(cmd).await?;
            parse::parse_f64(cmd, &raw)
        }

        Ok(Self {
            min_freq_hz: num(client, "DEV:INF:LIM:MINF?").await?,
            max_freq_hz: num(client, "DEV:INF:LIM:MAXF?").await?,
            min_ifbw_hz: num(client, "DEV:INF:LIM:MINIFBW?").await?,
            max_ifbw_hz: num(client, "DEV:INF:LIM:MAXIFBW?").await?,
            max_points: num(client, "DEV:INF:LIM:MAXPOINTS?").await? as usize,
            min_power_dbm: num(client, "DEV:INF:LIM:MINPOW?").await?,
            max_power_dbm: num(client, "DEV:INF:LIM:MAXPOW?").await?,
            min_rbw_hz: num(client, "DEV:INF:LIM:MINRBW?").await?,
            max_rbw_hz: num(client, "DEV:INF:LIM:MAXRBW?").await?,
        })
    }

    fn check(
        parameter: &str,
        value: f64,
        min: f64,
        max: f64,
        unit: &'static str,
    ) -> std::result::Result<(), SafetyError> {
        if value.is_nan() || value < min || value > max {
            return Err(SafetyError::OutOfRange {
                parameter: parameter.to_string(),
                value,
                min,
                max,
                unit,
            });
        }
        Ok(())
    }

    pub fn check_frequency(&self, name: &str, hz: f64) -> std::result::Result<(), SafetyError> {
        Self::check(name, hz, self.min_freq_hz, self.max_freq_hz, "Hz")
    }

    pub fn check_ifbw(&self, hz: f64) -> std::result::Result<(), SafetyError> {
        Self::check("IF bandwidth", hz, self.min_ifbw_hz, self.max_ifbw_hz, "Hz")
    }

    pub fn check_rbw(&self, hz: f64) -> std::result::Result<(), SafetyError> {
        Self::check(
            "resolution bandwidth",
            hz,
            self.min_rbw_hz,
            self.max_rbw_hz,
            "Hz",
        )
    }

    pub fn check_power(&self, dbm: f64) -> std::result::Result<(), SafetyError> {
        Self::check("power", dbm, self.min_power_dbm, self.max_power_dbm, "dBm")
    }

    pub fn check_points(&self, points: usize) -> std::result::Result<(), SafetyError> {
        Self::check(
            "sweep points",
            points as f64,
            1.0,
            self.max_points as f64,
            "points",
        )
    }

    /// Check that a start/stop pair is both in range and correctly ordered.
    pub fn check_span(&self, start_hz: f64, stop_hz: f64) -> std::result::Result<(), SafetyError> {
        self.check_frequency("start frequency", start_hz)?;
        self.check_frequency("stop frequency", stop_hz)?;
        if stop_hz < start_hz {
            return Err(SafetyError::OutOfRange {
                parameter: "stop frequency".to_string(),
                value: stop_hz,
                min: start_hz,
                max: self.max_freq_hz,
                unit: "Hz",
            });
        }
        Ok(())
    }
}

/// Live error flags read back from the instrument after an acquisition.
///
/// These accompany every measurement result. An overloaded ADC or an unlocked
/// PLL yields data that looks entirely plausible but is wrong, so the numbers
/// are never reported without them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, JsonSchema)]
pub struct StatusFlags {
    /// An input exceeded the ADC's range; readings are clipped.
    pub adc_overload: bool,
    /// A PLL lost lock; frequencies are not trustworthy.
    pub pll_unlocked: bool,
    /// The source could not reach the requested level.
    pub unlevel: bool,
}

impl StatusFlags {
    /// Read the three device status flags.
    ///
    /// The subsystem is `DEV:STA`, not the `DEV:STAT` the rest of the tree
    /// (`DEV:INF:...`) suggests. The instrument answers the wrong spelling with
    /// silence, so it fails as a timeout rather than an error.
    pub async fn query(client: &mut ScpiClient) -> Result<Self> {
        async fn flag(client: &mut ScpiClient, cmd: &str) -> Result<bool> {
            let raw = client.query(cmd).await?;
            parse::parse_bool(cmd, &raw)
        }

        Ok(Self {
            adc_overload: flag(client, "DEV:STA:ADCOVERLOAD?").await?,
            pll_unlocked: flag(client, "DEV:STA:UNLOCKED?").await?,
            unlevel: flag(client, "DEV:STA:UNLEVEL?").await?,
        })
    }

    /// Whether any flag is raised.
    pub fn any(&self) -> bool {
        self.adc_overload || self.pll_unlocked || self.unlevel
    }

    /// A human-readable warning, if anything is wrong.
    pub fn warning(&self) -> Option<String> {
        if !self.any() {
            return None;
        }
        let mut causes = Vec::new();
        if self.adc_overload {
            causes.push("ADC overload (input too strong, readings are clipped)");
        }
        if self.pll_unlocked {
            causes.push("PLL unlocked (frequencies are not trustworthy)");
        }
        if self.unlevel {
            causes.push("source unlevelled (stimulus did not reach the requested power)");
        }
        Some(format!(
            "Measurement is not trustworthy: {}.",
            causes.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> DeviceLimits {
        DeviceLimits {
            min_freq_hz: 100_000.0,
            max_freq_hz: 6.0e9,
            min_ifbw_hz: 10.0,
            max_ifbw_hz: 50_000.0,
            max_points: 4501,
            min_power_dbm: -40.0,
            max_power_dbm: 0.0,
            min_rbw_hz: 10.0,
            max_rbw_hz: 5.0e6,
        }
    }

    #[test]
    fn frequencies_at_the_boundary_are_allowed() {
        let l = limits();
        assert!(l.check_frequency("start", 100_000.0).is_ok());
        assert!(l.check_frequency("stop", 6.0e9).is_ok());
    }

    #[test]
    fn frequencies_outside_the_envelope_are_refused() {
        let l = limits();
        assert!(l.check_frequency("start", 99_999.0).is_err());
        assert!(l.check_frequency("stop", 6.000_000_1e9).is_err());
    }

    #[test]
    fn nan_is_refused_rather_than_slipping_through_comparisons() {
        // Every NaN comparison is false, so a naive range check would accept it.
        let l = limits();
        assert!(l.check_frequency("start", f64::NAN).is_err());
        assert!(l.check_power(f64::NAN).is_err());
    }

    #[test]
    fn inverted_spans_are_refused() {
        let l = limits();
        assert!(l.check_span(1.0e9, 2.0e9).is_ok());
        assert!(l.check_span(2.0e9, 1.0e9).is_err());
        // A zero-width span is legitimate: it is how zero-span mode is set.
        assert!(l.check_span(1.0e9, 1.0e9).is_ok());
    }

    #[test]
    fn point_counts_respect_the_reported_maximum() {
        let l = limits();
        assert!(l.check_points(4501).is_ok());
        assert!(l.check_points(4502).is_err());
        assert!(l.check_points(0).is_err());
    }

    #[test]
    fn out_of_range_errors_name_the_parameter_and_the_bounds() {
        let l = limits();
        let msg = l.check_power(5.0).unwrap_err().to_string();
        assert!(msg.contains("power"), "{msg}");
        assert!(msg.contains("-40"), "should state the lower bound: {msg}");
        assert!(msg.contains('0'), "should state the upper bound: {msg}");
    }

    #[test]
    fn status_flags_are_quiet_when_healthy() {
        assert!(StatusFlags::default().warning().is_none());
        assert!(!StatusFlags::default().any());
    }

    #[test]
    fn status_flags_explain_each_fault() {
        let flags = StatusFlags {
            adc_overload: true,
            pll_unlocked: true,
            unlevel: false,
        };
        let warning = flags.warning().expect("faults should produce a warning");
        assert!(warning.contains("ADC overload"), "{warning}");
        assert!(warning.contains("PLL unlocked"), "{warning}");
        assert!(!warning.contains("unlevelled"), "{warning}");
    }

    #[test]
    fn modes_round_trip_through_scpi_spelling() {
        for mode in [Mode::Vna, Mode::Sa, Mode::Gen] {
            assert_eq!(Mode::parse(mode.as_scpi()), Some(mode));
        }
        assert_eq!(Mode::parse("vna"), Some(Mode::Vna));
        assert_eq!(Mode::parse("nonsense"), None);
    }
}
