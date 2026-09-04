//! Parsers for LibreVNA SCPI responses.
//!
//! These are deliberately tolerant. The exact wire format of the trace-data
//! responses is documented only loosely upstream (as "`[x, real, imag]`
//! tuples"), and the bracketing and delimiters have not yet been confirmed
//! against hardware. Keeping every format assumption in this one module means
//! correcting it later is a local change.

use crate::error::{Result, VnaError};

/// Reported magnitude of a zero reading, in dB.
///
/// Far below the noise floor of any real VNA (the LibreVNA manages roughly
/// -120 dB at best), so it cannot be mistaken for a measurement, while keeping
/// every value finite and therefore representable in JSON.
pub const FLOOR_DB: f64 = -400.0;

/// A single complex measurement point: an x-axis value plus a complex reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComplexPoint {
    /// Frequency in Hz, power in dBm, or time in seconds depending on sweep type.
    pub x: f64,
    pub re: f64,
    pub im: f64,
}

impl ComplexPoint {
    /// Magnitude in dB (`20·log10|s|`).
    ///
    /// An exactly-zero reading is reported as [`FLOOR_DB`] rather than
    /// negative infinity. JSON cannot represent infinity, so it would reach the
    /// agent as a bare `null` -- indistinguishable from a missing field, and it
    /// would silently poison the bandwidth and Q calculations downstream. A
    /// finite floor keeps the value sortable, serialisable and obviously
    /// below anything the hardware can actually measure.
    pub fn magnitude_db(&self) -> f64 {
        let mag = self.re.hypot(self.im);
        if mag <= 0.0 {
            FLOOR_DB
        } else {
            (20.0 * mag.log10()).max(FLOOR_DB)
        }
    }

    /// Phase in degrees, in `(-180, 180]`.
    pub fn phase_deg(&self) -> f64 {
        self.im.atan2(self.re).to_degrees()
    }
}

/// A single scalar measurement point, as returned by the spectrum analyser.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScalarPoint {
    pub x: f64,
    /// Level in dBm.
    pub y: f64,
}

/// Parse a SCPI boolean.
///
/// LibreVNA answers with `TRUE`/`FALSE`, but accepts and sometimes emits
/// `1`/`0`, so both are handled.
pub fn parse_bool(command: &str, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "TRUE" | "1" => Ok(true),
        "FALSE" | "0" => Ok(false),
        _ => Err(VnaError::Parse {
            command: command.to_string(),
            reason: "expected TRUE, FALSE, 1 or 0".into(),
            raw: raw.to_string(),
        }),
    }
}

/// Parse a SCPI floating-point scalar.
pub fn parse_f64(command: &str, raw: &str) -> Result<f64> {
    let trimmed = raw.trim();
    trimmed.parse::<f64>().map_err(|e| VnaError::Parse {
        command: command.to_string(),
        reason: format!("expected a number: {e}"),
        raw: raw.to_string(),
    })
}

/// Parse a SCPI integer scalar.
///
/// Accepts a float-formatted integer (`4501.000000`) because LibreVNA is not
/// consistent about which it emits for count-valued queries.
pub fn parse_usize(command: &str, raw: &str) -> Result<usize> {
    let trimmed = raw.trim();
    if let Ok(n) = trimmed.parse::<usize>() {
        return Ok(n);
    }
    let as_float = parse_f64(command, trimmed)?;
    if as_float < 0.0 || as_float.fract() != 0.0 {
        return Err(VnaError::Parse {
            command: command.to_string(),
            reason: "expected a non-negative whole number".into(),
            raw: raw.to_string(),
        });
    }
    Ok(as_float as usize)
}

/// Parse a comma-separated list, such as the trace or device-serial lists.
///
/// An empty response is an empty list, not an error: a device with no traces
/// defined is a normal state.
pub fn parse_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Split a numeric response into its component values, ignoring any bracketing.
///
/// Accepts `[1,2,3],[4,5,6]`, `1,2,3,4,5,6` and whitespace-padded variants
/// alike, since the exact framing is not yet confirmed.
fn split_numbers(command: &str, raw: &str) -> Result<Vec<f64>> {
    raw.split(['[', ']', ',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<f64>().map_err(|e| VnaError::Parse {
                command: command.to_string(),
                reason: format!("expected a number, found {s:?}: {e}"),
                raw: raw.to_string(),
            })
        })
        .collect()
}

/// Parse complex trace data as `(x, real, imag)` triples.
pub fn parse_complex_trace(command: &str, raw: &str) -> Result<Vec<ComplexPoint>> {
    let values = split_numbers(command, raw)?;
    if values.len() % 3 != 0 {
        return Err(VnaError::Parse {
            command: command.to_string(),
            reason: format!(
                "expected a multiple of 3 values for (x, real, imag) triples, got {}",
                values.len()
            ),
            raw: raw.to_string(),
        });
    }
    Ok(values
        .as_chunks::<3>()
        .0
        .iter()
        .map(|&[x, re, im]| ComplexPoint { x, re, im })
        .collect())
}

/// Parse scalar trace data as `(x, y)` pairs, as the spectrum analyser returns.
pub fn parse_scalar_trace(command: &str, raw: &str) -> Result<Vec<ScalarPoint>> {
    let values = split_numbers(command, raw)?;
    if values.len() % 2 != 0 {
        return Err(VnaError::Parse {
            command: command.to_string(),
            reason: format!(
                "expected an even number of values for (x, y) pairs, got {}",
                values.len()
            ),
            raw: raw.to_string(),
        });
    }
    Ok(values
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[x, y]| ScalarPoint { x, y })
        .collect())
}

/// Parse a single complex reading, as `VNA:TRAC:AT?` returns.
pub fn parse_complex_pair(command: &str, raw: &str) -> Result<(f64, f64)> {
    let values = split_numbers(command, raw)?;
    match values.as_slice() {
        [re, im] => Ok((*re, *im)),
        _ => Err(VnaError::Parse {
            command: command.to_string(),
            reason: format!(
                "expected exactly 2 values (real, imag), got {}",
                values.len()
            ),
            raw: raw.to_string(),
        }),
    }
}

/// Parse the `*IDN?` response into its four comma-separated fields.
///
/// Documented as `LibreVNA,LibreVNA-GUI,<serial>,<version>`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct Identity {
    pub manufacturer: String,
    pub model: String,
    pub serial: String,
    pub version: String,
}

pub fn parse_identity(raw: &str) -> Result<Identity> {
    let fields = parse_list(raw);
    if fields.len() < 4 {
        return Err(VnaError::Parse {
            command: "*IDN?".into(),
            reason: format!("expected 4 comma-separated fields, got {}", fields.len()),
            raw: raw.to_string(),
        });
    }
    Ok(Identity {
        manufacturer: fields[0].clone(),
        model: fields[1].clone(),
        serial: fields[2].clone(),
        version: fields[3].clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bools_accept_both_spellings() {
        for t in ["TRUE", "true", "1", " TRUE "] {
            assert!(parse_bool("X?", t).unwrap(), "{t:?} should be true");
        }
        for f in ["FALSE", "false", "0", "\tFALSE\r"] {
            assert!(!parse_bool("X?", f).unwrap(), "{f:?} should be false");
        }
    }

    #[test]
    fn bools_reject_anything_else() {
        // An error reply must not be silently read as `false`.
        for bad in ["", "ERROR", "YES", "2"] {
            assert!(parse_bool("X?", bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn floats_accept_scientific_and_padded() {
        assert_eq!(parse_f64("X?", "1e6").unwrap(), 1e6);
        assert_eq!(parse_f64("X?", " 1000000.000000\r\n").unwrap(), 1e6);
        assert_eq!(parse_f64("X?", "-31.75").unwrap(), -31.75);
    }

    #[test]
    fn counts_accept_float_formatting() {
        assert_eq!(parse_usize("X?", "4501").unwrap(), 4501);
        assert_eq!(parse_usize("X?", "4501.000000").unwrap(), 4501);
        assert!(parse_usize("X?", "4501.5").is_err());
        assert!(parse_usize("X?", "-1").is_err());
    }

    #[test]
    fn empty_list_is_empty_not_an_error() {
        assert!(parse_list("").is_empty());
        assert!(parse_list("   \r\n").is_empty());
    }

    #[test]
    fn lists_split_and_trim() {
        assert_eq!(parse_list("S11,S21"), vec!["S11", "S21"]);
        assert_eq!(parse_list(" a , b ,c "), vec!["a", "b", "c"]);
        // Trailing separator should not yield a phantom empty entry.
        assert_eq!(parse_list("a,b,"), vec!["a", "b"]);
    }

    #[test]
    fn complex_trace_parses_bracketed_form() {
        let pts = parse_complex_trace("T?", "[1e6,0.5,0.1],[2e6,0.4,0.2]").unwrap();
        assert_eq!(pts.len(), 2);
        assert_eq!(
            pts[0],
            ComplexPoint {
                x: 1e6,
                re: 0.5,
                im: 0.1
            }
        );
        assert_eq!(pts[1].x, 2e6);
    }

    #[test]
    fn complex_trace_parses_flat_form() {
        // The same data without brackets must parse identically -- we do not
        // yet know which form the instrument actually emits.
        let bracketed = parse_complex_trace("T?", "[1e6,0.5,0.1],[2e6,0.4,0.2]").unwrap();
        let flat = parse_complex_trace("T?", "1e6,0.5,0.1,2e6,0.4,0.2").unwrap();
        assert_eq!(bracketed, flat);
    }

    #[test]
    fn complex_trace_rejects_ragged_data() {
        // A truncated final tuple must be an error, never a silently dropped point.
        let err = parse_complex_trace("T?", "1e6,0.5,0.1,2e6,0.4").unwrap_err();
        assert!(matches!(err, VnaError::Parse { .. }));
    }

    #[test]
    fn complex_trace_reports_the_offending_token() {
        let err = parse_complex_trace("T?", "1e6,NaNsense,0.1").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("NaNsense"),
            "message should name the bad token: {msg}"
        );
    }

    #[test]
    fn scalar_trace_parses_pairs() {
        let pts = parse_scalar_trace("T?", "1e6,-40.0,2e6,-38.5").unwrap();
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[1], ScalarPoint { x: 2e6, y: -38.5 });
        assert!(parse_scalar_trace("T?", "1e6,-40.0,2e6").is_err());
    }

    #[test]
    fn magnitude_db_matches_known_values() {
        // |s| = 1 is 0 dB.
        let unity = ComplexPoint {
            x: 0.0,
            re: 1.0,
            im: 0.0,
        };
        assert!((unity.magnitude_db() - 0.0).abs() < 1e-12);

        // |s| = 0.1 is -20 dB.
        let tenth = ComplexPoint {
            x: 0.0,
            re: 0.1,
            im: 0.0,
        };
        assert!((tenth.magnitude_db() + 20.0).abs() < 1e-9);

        // A 3-4-5 triangle has magnitude 0.5.
        let mixed = ComplexPoint {
            x: 0.0,
            re: 0.3,
            im: 0.4,
        };
        assert!((mixed.magnitude_db() - 20.0 * 0.5f64.log10()).abs() < 1e-9);
    }

    #[test]
    fn magnitude_db_of_zero_is_a_finite_floor_not_infinity() {
        // Infinity would serialise to JSON `null` and reach the agent as a
        // missing field, so a zero reading must land on a finite floor.
        let null = ComplexPoint {
            x: 0.0,
            re: 0.0,
            im: 0.0,
        };
        assert_eq!(null.magnitude_db(), FLOOR_DB);
        assert!(null.magnitude_db().is_finite());
        // It must still sort below any real measurement.
        let weak = ComplexPoint {
            x: 0.0,
            re: 1e-9,
            im: 0.0,
        };
        assert!(null.magnitude_db() < weak.magnitude_db());
    }

    #[test]
    fn every_magnitude_is_json_representable() {
        for point in [
            ComplexPoint {
                x: 0.0,
                re: 0.0,
                im: 0.0,
            },
            ComplexPoint {
                x: 0.0,
                re: 1.0,
                im: 0.0,
            },
            ComplexPoint {
                x: 0.0,
                re: -1e-300,
                im: 0.0,
            },
        ] {
            let db = point.magnitude_db();
            assert!(db.is_finite(), "{point:?} produced {db}");
            assert!(serde_json::to_string(&db).unwrap() != "null");
        }
    }

    #[test]
    fn phase_is_in_degrees() {
        let quarter = ComplexPoint {
            x: 0.0,
            re: 0.0,
            im: 1.0,
        };
        assert!((quarter.phase_deg() - 90.0).abs() < 1e-9);
    }

    #[test]
    fn identity_splits_into_four_fields() {
        let id = parse_identity("LibreVNA,LibreVNA-GUI,00A1B2C3,1.4.0").unwrap();
        assert_eq!(id.serial, "00A1B2C3");
        assert_eq!(id.version, "1.4.0");
        assert!(parse_identity("LibreVNA,LibreVNA-GUI").is_err());
    }
}
