//! Derived measurements: the numbers an agent actually reasons about.
//!
//! A full sweep is thousands of points. Returning all of them buries the
//! result, so the tools return these summaries instead and write the complete
//! data to a Touchstone file. Everything here is a pure function of a point
//! series, which is what makes it worth testing carefully -- a wrong resonance
//! or a hidden notch would be reported with total confidence.

use schemars::JsonSchema;
use serde::Serialize;

use crate::scpi::parse::{ComplexPoint, ScalarPoint};

/// One bucket of an envelope-decimated trace.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
pub struct EnvelopeBin {
    /// Centre frequency of the bucket, in Hz.
    pub x: f64,
    /// Lowest value seen in the bucket, in dB.
    pub min_db: f64,
    /// Highest value seen in the bucket, in dB.
    pub max_db: f64,
}

/// A located extreme in a trace.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
pub struct Extreme {
    pub freq_hz: f64,
    pub magnitude_db: f64,
}

/// Summary of a reflection trace: where it resonates and how sharply.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ResonanceSummary {
    /// The deepest point of the trace.
    pub resonance: Extreme,
    /// Voltage standing wave ratio at resonance.
    pub vswr: f64,
    /// Return loss at resonance, in dB (positive by convention).
    pub return_loss_db: f64,
    /// Width of the region within `depth_db` of the minimum, in Hz.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bandwidth_hz: Option<f64>,
    /// `f0 / bandwidth_hz`, absent when the bandwidth is not resolvable.
    ///
    /// This is the Q of the *feature measured at `depth_db`*, not necessarily
    /// the resonator's loaded Q: for a deep notch the two differ considerably,
    /// because the width a few dB above a deep minimum is far narrower than the
    /// resonator's own 3 dB bandwidth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<f64>,
    /// The depth threshold the bandwidth was measured at, in dB.
    pub depth_db: f64,
    /// True when the feature is too narrow for this sweep to characterise.
    ///
    /// A bandwidth spanning only a sample or two is an artefact of the point
    /// spacing, not a measurement: the real notch could be far narrower or a
    /// different shape entirely. The frequency and depth are still meaningful;
    /// the bandwidth and Q are not.
    pub under_resolved: bool,
    /// Spacing between swept points near the feature, in Hz.
    pub sample_spacing_hz: f64,
}

/// The largest VSWR reported, standing in for total reflection.
///
/// Total reflection is genuinely infinite VSWR, but infinity serialises to
/// JSON `null`, which would reach the agent as a missing value rather than as
/// "this port is completely unmatched". Capping keeps the meaning.
pub const MAX_VSWR: f64 = 9999.0;

/// Convert a reflection magnitude in dB to VSWR.
///
/// Saturates at [`MAX_VSWR`] for total reflection (0 dB and above) rather than
/// returning infinity, so the result always survives the trip through JSON.
pub fn vswr_from_db(magnitude_db: f64) -> f64 {
    if magnitude_db.is_nan() {
        return MAX_VSWR;
    }
    let gamma = 10f64.powf(magnitude_db / 20.0);
    if gamma >= 1.0 {
        return MAX_VSWR;
    }
    ((1.0 + gamma) / (1.0 - gamma)).min(MAX_VSWR)
}

/// Locate the lowest-magnitude point of a trace.
pub fn find_minimum(points: &[ComplexPoint]) -> Option<Extreme> {
    points
        .iter()
        .map(|p| Extreme {
            freq_hz: p.x,
            magnitude_db: p.magnitude_db(),
        })
        .min_by(|a, b| a.magnitude_db.total_cmp(&b.magnitude_db))
}

/// Locate the highest-magnitude point of a trace.
pub fn find_maximum(points: &[ComplexPoint]) -> Option<Extreme> {
    points
        .iter()
        .map(|p| Extreme {
            freq_hz: p.x,
            magnitude_db: p.magnitude_db(),
        })
        .max_by(|a, b| a.magnitude_db.total_cmp(&b.magnitude_db))
}

/// Interpolate the magnitude of a trace at an arbitrary frequency.
///
/// Returns `None` outside the swept range rather than extrapolating: an
/// invented value beyond the sweep would be indistinguishable from a measured
/// one in the result.
pub fn magnitude_at(points: &[ComplexPoint], freq_hz: f64) -> Option<f64> {
    if points.len() < 2 {
        return points
            .first()
            .filter(|p| p.x == freq_hz)
            .map(|p| p.magnitude_db());
    }
    let first = points.first()?;
    let last = points.last()?;
    if freq_hz < first.x || freq_hz > last.x {
        return None;
    }

    let idx = points.partition_point(|p| p.x < freq_hz);
    if idx == 0 {
        return Some(points[0].magnitude_db());
    }
    let (lo, hi) = (&points[idx - 1], &points[idx]);
    if hi.x == lo.x {
        return Some(lo.magnitude_db());
    }
    let t = (freq_hz - lo.x) / (hi.x - lo.x);
    Some(lo.magnitude_db() + t * (hi.magnitude_db() - lo.magnitude_db()))
}

/// Measure the width of the dip around the trace minimum.
///
/// Finds where the response rises `depth_db` above its minimum on either side,
/// interpolating between samples. Returns `None` if the trace never rises that
/// far within the sweep -- the span is then too narrow to characterise the
/// feature, and guessing would be worse than saying so.
pub fn bandwidth_around_minimum(points: &[ComplexPoint], depth_db: f64) -> Option<f64> {
    if points.len() < 3 {
        return None;
    }
    let minimum = find_minimum(points)?;
    if minimum.magnitude_db <= crate::scpi::parse::FLOOR_DB {
        // A floored reading carries no shape to measure a width against.
        return None;
    }
    let threshold = minimum.magnitude_db + depth_db;

    let centre = points.iter().position(|p| p.x == minimum.freq_hz)?;

    let left = crossing_below(points, centre, threshold, Direction::Down)?;
    let right = crossing_below(points, centre, threshold, Direction::Up)?;
    Some(right - left)
}

enum Direction {
    Down,
    Up,
}

/// Walk outward from `centre` until the trace crosses `threshold`, and
/// interpolate the exact crossing frequency.
fn crossing_below(
    points: &[ComplexPoint],
    centre: usize,
    threshold: f64,
    direction: Direction,
) -> Option<f64> {
    let mut previous = centre;
    let mut index = centre;
    loop {
        let next = match direction {
            Direction::Down => index.checked_sub(1)?,
            Direction::Up => {
                let n = index + 1;
                if n >= points.len() {
                    return None;
                }
                n
            }
        };
        let value = points[next].magnitude_db();
        if value >= threshold {
            let before = points[previous].magnitude_db();
            let span = value - before;
            let t = if span.abs() < f64::EPSILON {
                0.0
            } else {
                (threshold - before) / span
            };
            return Some(points[previous].x + t * (points[next].x - points[previous].x));
        }
        previous = next;
        index = next;
    }
}

/// How many sample intervals a feature must span to be considered resolved.
///
/// Below this, the interpolated width is dominated by where the samples happen
/// to fall rather than by the shape of the feature itself.
const MIN_SPACINGS_TO_RESOLVE: f64 = 3.0;

/// Mean spacing between swept points, in Hz.
fn sample_spacing(points: &[ComplexPoint]) -> f64 {
    match (points.first(), points.last()) {
        (Some(first), Some(last)) if points.len() > 1 => {
            (last.x - first.x).abs() / (points.len() - 1) as f64
        }
        _ => 0.0,
    }
}

/// Summarise a reflection trace.
pub fn summarise_resonance(points: &[ComplexPoint], depth_db: f64) -> Option<ResonanceSummary> {
    let resonance = find_minimum(points)?;
    let bandwidth_hz = bandwidth_around_minimum(points, depth_db);
    let q = bandwidth_hz
        .filter(|bw| *bw > 0.0)
        .map(|bw| resonance.freq_hz / bw);

    let sample_spacing_hz = sample_spacing(points);
    let under_resolved = match bandwidth_hz {
        Some(bw) => sample_spacing_hz > 0.0 && bw < sample_spacing_hz * MIN_SPACINGS_TO_RESOLVE,
        None => false,
    };

    Some(ResonanceSummary {
        vswr: vswr_from_db(resonance.magnitude_db),
        return_loss_db: -resonance.magnitude_db,
        resonance,
        bandwidth_hz,
        q,
        depth_db,
        under_resolved,
        sample_spacing_hz,
    })
}

impl ResonanceSummary {
    /// A warning if the bandwidth and Q should not be relied upon.
    pub fn warning(&self) -> Option<String> {
        if !self.under_resolved {
            return None;
        }
        Some(format!(
            "The feature at {:.6} MHz spans less than {:.0} sweep points \
             (measured width {:.1} kHz against {:.1} kHz point spacing), so the \
             bandwidth and Q are artefacts of the sampling rather than \
             measurements. Narrow the span around the resonance and sweep again \
             to characterise it.",
            self.resonance.freq_hz / 1e6,
            MIN_SPACINGS_TO_RESOLVE,
            self.bandwidth_hz.unwrap_or(0.0) / 1e3,
            self.sample_spacing_hz / 1e3,
        ))
    }
}

/// Reduce a trace to at most `buckets` min/max bins.
///
/// Deliberately *not* stride sampling. A narrow notch is usually the whole
/// point of a reflection measurement, and plain subsampling would step straight
/// over it; keeping both extremes of each bucket cannot lose one.
pub fn envelope_decimate(points: &[ComplexPoint], buckets: usize) -> Vec<EnvelopeBin> {
    if points.is_empty() || buckets == 0 {
        return Vec::new();
    }
    if points.len() <= buckets {
        return points
            .iter()
            .map(|p| {
                let db = p.magnitude_db();
                EnvelopeBin {
                    x: p.x,
                    min_db: db,
                    max_db: db,
                }
            })
            .collect();
    }

    let mut bins = Vec::with_capacity(buckets);
    for bucket in 0..buckets {
        let start = bucket * points.len() / buckets;
        let end = ((bucket + 1) * points.len() / buckets).max(start + 1);
        let slice = &points[start..end.min(points.len())];
        if slice.is_empty() {
            continue;
        }

        let mut min_db = f64::INFINITY;
        let mut max_db = f64::NEG_INFINITY;
        for point in slice {
            let db = point.magnitude_db();
            min_db = min_db.min(db);
            max_db = max_db.max(db);
        }
        bins.push(EnvelopeBin {
            x: (slice[0].x + slice[slice.len() - 1].x) / 2.0,
            min_db,
            max_db,
        });
    }
    bins
}

/// A located spectral peak.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
pub struct Peak {
    pub freq_hz: f64,
    pub level_dbm: f64,
}

/// Find spectral peaks above `floor_dbm`, strongest first.
///
/// A point qualifies only if it is a local maximum *and* no stronger peak lies
/// within `min_separation_hz`, so a single broad carrier yields one entry
/// rather than a cluster of samples along its skirt.
pub fn find_peaks(
    points: &[ScalarPoint],
    floor_dbm: f64,
    min_separation_hz: f64,
    limit: usize,
) -> Vec<Peak> {
    // A point partway up a carrier's skirt is above the floor but is not a
    // signal; requiring a genuine local maximum keeps the table meaningful.
    let mut candidates: Vec<Peak> = points
        .iter()
        .enumerate()
        .filter(|(_, p)| p.y >= floor_dbm)
        .filter(|(i, p)| {
            let rising = *i == 0 || points[i - 1].y <= p.y;
            let falling = *i + 1 >= points.len() || points[i + 1].y <= p.y;
            rising && falling
        })
        .map(|(_, p)| Peak {
            freq_hz: p.x,
            level_dbm: p.y,
        })
        .collect();

    // Strongest first, so the greedy pass below keeps the tallest of each group.
    candidates.sort_by(|a, b| b.level_dbm.total_cmp(&a.level_dbm));

    let mut kept: Vec<Peak> = Vec::new();
    for candidate in candidates {
        if kept.len() >= limit {
            break;
        }
        let too_close = kept
            .iter()
            .any(|k| (k.freq_hz - candidate.freq_hz).abs() < min_separation_hz);
        if !too_close {
            kept.push(candidate);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trace that is flat at 0 dB except for a notch of `depth_db` at `notch`.
    fn notched_trace(points: usize, notch: usize, depth_db: f64) -> Vec<ComplexPoint> {
        (0..points)
            .map(|i| {
                let magnitude = if i == notch {
                    10f64.powf(depth_db / 20.0)
                } else {
                    1.0
                };
                ComplexPoint {
                    x: 1.0e9 + i as f64 * 1.0e6,
                    re: magnitude,
                    im: 0.0,
                }
            })
            .collect()
    }

    /// A symmetric V-shaped dip, so bandwidth has an exact analytic answer.
    fn v_shaped_dip() -> Vec<ComplexPoint> {
        // Magnitudes in dB: -0, -10, -20, -10, -0 at 1 MHz spacing.
        [0.0, -10.0, -20.0, -10.0, 0.0]
            .iter()
            .enumerate()
            .map(|(i, db)| ComplexPoint {
                x: 1.0e9 + i as f64 * 1.0e6,
                re: 10f64.powf(db / 20.0),
                im: 0.0,
            })
            .collect()
    }

    #[test]
    fn vswr_matches_textbook_values() {
        // -10 dB return loss is the standard 1.925 : 1.
        assert!(
            (vswr_from_db(-10.0) - 1.9250).abs() < 1e-3,
            "{}",
            vswr_from_db(-10.0)
        );
        // -20 dB is 1.222 : 1.
        assert!((vswr_from_db(-20.0) - 1.2222).abs() < 1e-3);
        // A perfect match is 1 : 1.
        assert!((vswr_from_db(-1000.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn total_reflection_saturates_rather_than_returning_infinity() {
        // Infinity would serialise to JSON `null` and read as a missing value.
        assert_eq!(vswr_from_db(0.0), MAX_VSWR);
        assert_eq!(vswr_from_db(3.0), MAX_VSWR);
        assert!(vswr_from_db(0.0).is_finite());
    }

    #[test]
    fn every_vswr_survives_json() {
        for db in [-400.0, -100.0, -10.0, -0.001, 0.0, 10.0, f64::NAN] {
            let vswr = vswr_from_db(db);
            assert!(vswr.is_finite(), "{db} dB gave {vswr}");
            assert_ne!(serde_json::to_string(&vswr).unwrap(), "null");
        }
    }

    #[test]
    fn finds_the_minimum_of_a_trace() {
        let trace = notched_trace(101, 40, -30.0);
        let min = find_minimum(&trace).unwrap();
        assert_eq!(min.freq_hz, 1.0e9 + 40.0e6);
        assert!((min.magnitude_db + 30.0).abs() < 1e-9);
    }

    #[test]
    fn empty_traces_yield_no_extremes() {
        assert!(find_minimum(&[]).is_none());
        assert!(find_maximum(&[]).is_none());
        assert!(summarise_resonance(&[], 3.0).is_none());
    }

    #[test]
    fn envelope_decimation_preserves_a_single_point_notch() {
        // The whole reason for min/max bucketing: a stride sample would step
        // over this notch entirely and report a flat, healthy trace.
        let trace = notched_trace(1000, 617, -40.0);
        let bins = envelope_decimate(&trace, 50);

        assert!(bins.len() <= 50);
        let deepest = bins.iter().map(|b| b.min_db).fold(f64::INFINITY, f64::min);
        assert!(
            (deepest + 40.0).abs() < 1e-9,
            "the -40 dB notch must survive decimation, deepest bin was {deepest}"
        );
    }

    #[test]
    fn envelope_decimation_covers_the_whole_sweep() {
        let trace = notched_trace(1000, 500, -40.0);
        let bins = envelope_decimate(&trace, 64);
        assert!(bins.first().unwrap().x < trace[10].x);
        assert!(bins.last().unwrap().x > trace[trace.len() - 10].x);
    }

    #[test]
    fn short_traces_pass_through_decimation_unchanged() {
        let trace = notched_trace(10, 5, -20.0);
        let bins = envelope_decimate(&trace, 50);
        assert_eq!(bins.len(), 10);
        assert_eq!(bins[5].min_db, bins[5].max_db);
    }

    #[test]
    fn decimation_of_nothing_is_nothing() {
        assert!(envelope_decimate(&[], 10).is_empty());
        assert!(envelope_decimate(&notched_trace(10, 1, -3.0), 0).is_empty());
    }

    #[test]
    fn bandwidth_interpolates_between_samples() {
        // Minimum -20 dB at 1002 MHz; the -10 dB threshold falls exactly on the
        // neighbouring samples, so the width is exactly 2 MHz.
        let trace = v_shaped_dip();
        let bw = bandwidth_around_minimum(&trace, 10.0).unwrap();
        assert!((bw - 2.0e6).abs() < 1.0, "expected 2 MHz, got {bw}");
    }

    #[test]
    fn bandwidth_is_absent_when_the_dip_never_recovers() {
        // A monotonic ramp has a minimum at the edge and never rises again on
        // that side, so no width can honestly be reported.
        let trace: Vec<ComplexPoint> = (0..50)
            .map(|i| ComplexPoint {
                x: 1.0e9 + i as f64 * 1.0e6,
                re: 10f64.powf(-(50.0 - i as f64) / 20.0),
                im: 0.0,
            })
            .collect();
        assert!(bandwidth_around_minimum(&trace, 3.0).is_none());
    }

    #[test]
    fn a_notch_spanning_a_couple_of_points_is_flagged_under_resolved() {
        // A single-point notch in a coarse sweep: the interpolated width is an
        // artefact of where the samples fell, not a measurement.
        let trace = notched_trace(101, 50, -40.0);
        let summary = summarise_resonance(&trace, 3.0).unwrap();
        assert!(
            summary.under_resolved,
            "a one-point notch must be flagged, got {summary:?}"
        );
        let warning = summary.warning().expect("under-resolved must warn");
        assert!(warning.contains("Narrow the span"), "{warning}");
    }

    #[test]
    fn a_well_sampled_feature_is_not_flagged() {
        // A broad dip spread over many points is genuinely resolved.
        let trace: Vec<ComplexPoint> = (0..401)
            .map(|i| {
                let f = 1.0e9 + i as f64 * 1.0e5;
                let detune = (f - 1.02e9) / 5.0e6;
                let magnitude = (detune * detune / (1.0 + detune * detune)).max(1e-3);
                ComplexPoint {
                    x: f,
                    re: magnitude,
                    im: 0.0,
                }
            })
            .collect();
        let summary = summarise_resonance(&trace, 3.0).unwrap();
        assert!(
            !summary.under_resolved,
            "a broad dip should not be flagged, got {summary:?}"
        );
        assert!(summary.warning().is_none());
    }

    #[test]
    fn sample_spacing_is_reported_so_the_caller_can_judge() {
        let trace = notched_trace(101, 50, -20.0);
        let summary = summarise_resonance(&trace, 3.0).unwrap();
        assert!((summary.sample_spacing_hz - 1.0e6).abs() < 1.0);
    }

    #[test]
    fn resonance_summary_reports_q_and_return_loss() {
        let trace = v_shaped_dip();
        let summary = summarise_resonance(&trace, 10.0).unwrap();
        assert!((summary.return_loss_db - 20.0).abs() < 1e-6);
        assert_eq!(summary.resonance.freq_hz, 1.002e9);
        let q = summary.q.expect("a resolvable dip should yield a Q");
        // f0 / BW = 1002 MHz / 2 MHz.
        assert!((q - 501.0).abs() < 1.0, "expected Q near 501, got {q}");
    }

    #[test]
    fn magnitude_interpolates_within_the_sweep() {
        let trace = v_shaped_dip();
        // Halfway between the 0 dB and -10 dB samples.
        let mid = magnitude_at(&trace, 1.0005e9).unwrap();
        assert!((mid + 5.0).abs() < 1e-6, "expected -5 dB, got {mid}");
    }

    #[test]
    fn magnitude_outside_the_sweep_is_absent_rather_than_extrapolated() {
        let trace = v_shaped_dip();
        assert!(magnitude_at(&trace, 0.5e9).is_none());
        assert!(magnitude_at(&trace, 9.0e9).is_none());
        // The exact endpoints remain available.
        assert!(magnitude_at(&trace, 1.0e9).is_some());
        assert!(magnitude_at(&trace, 1.004e9).is_some());
    }

    #[test]
    fn peaks_are_grouped_not_reported_per_sample() {
        // A broad carrier spanning many samples should yield one peak.
        let points: Vec<ScalarPoint> = (0..201)
            .map(|i| {
                let f = 1.0e9 + i as f64 * 1.0e6;
                let offset = (f - 1.1e9) / 5.0e6;
                ScalarPoint {
                    x: f,
                    y: -95.0 + 60.0 / (1.0 + offset * offset),
                }
            })
            .collect();

        let peaks = find_peaks(&points, -60.0, 20.0e6, 10);
        assert_eq!(peaks.len(), 1, "expected one grouped peak, got {peaks:?}");
        assert!((peaks[0].freq_hz - 1.1e9).abs() < 2.0e6);
    }

    #[test]
    fn points_on_a_carriers_skirt_are_not_reported_as_signals() {
        // Samples partway up a single carrier are above the floor but are not
        // separate signals; only the summit is.
        let points: Vec<ScalarPoint> = (0..201)
            .map(|i| {
                let f = 1.0e9 + i as f64 * 1.0e6;
                let offset = (f - 1.1e9) / 3.0e6;
                ScalarPoint {
                    x: f,
                    y: -95.0 + 65.0 / (1.0 + offset * offset),
                }
            })
            .collect();

        // A separation of zero disables grouping, so only the local-maximum
        // rule can keep the skirt out.
        let peaks = find_peaks(&points, -90.0, 0.0, 10);
        assert_eq!(peaks.len(), 1, "only the summit is a signal: {peaks:?}");
        assert!((peaks[0].freq_hz - 1.1e9).abs() < 1.0e6);
    }

    #[test]
    fn two_genuinely_separate_carriers_are_both_reported() {
        let points: Vec<ScalarPoint> = (0..401)
            .map(|i| {
                let f = 1.0e9 + i as f64 * 1.0e6;
                let a = (f - 1.1e9) / 2.0e6;
                let b = (f - 1.3e9) / 2.0e6;
                ScalarPoint {
                    x: f,
                    y: -95.0 + 60.0 / (1.0 + a * a) + 50.0 / (1.0 + b * b),
                }
            })
            .collect();

        let peaks = find_peaks(&points, -80.0, 10.0e6, 10);
        assert_eq!(peaks.len(), 2, "both carriers should appear: {peaks:?}");
        // Strongest first.
        assert!((peaks[0].freq_hz - 1.1e9).abs() < 2.0e6);
        assert!((peaks[1].freq_hz - 1.3e9).abs() < 2.0e6);
    }

    #[test]
    fn peaks_below_the_floor_are_ignored() {
        let points: Vec<ScalarPoint> = (0..50)
            .map(|i| ScalarPoint {
                x: 1.0e9 + i as f64 * 1.0e6,
                y: -95.0,
            })
            .collect();
        assert!(find_peaks(&points, -60.0, 1.0e6, 10).is_empty());
    }

    #[test]
    fn peak_results_are_strongest_first_and_respect_the_limit() {
        // Five distinct carriers of descending strength, 100 MHz apart.
        let points: Vec<ScalarPoint> = (0..600)
            .map(|i| {
                let f = 1.0e9 + i as f64 * 1.0e6;
                let level = (0..5)
                    .map(|c| {
                        let centre = 1.05e9 + c as f64 * 100.0e6;
                        let offset = (f - centre) / 2.0e6;
                        (60.0 - c as f64 * 5.0) / (1.0 + offset * offset)
                    })
                    .sum::<f64>();
                ScalarPoint {
                    x: f,
                    y: -100.0 + level,
                }
            })
            .collect();

        let peaks = find_peaks(&points, -80.0, 10.0e6, 3);
        assert_eq!(peaks.len(), 3, "the limit should cap the table: {peaks:?}");
        assert!(peaks[0].level_dbm > peaks[1].level_dbm);
        assert!(peaks[1].level_dbm > peaks[2].level_dbm);
        // Strongest carrier first.
        assert!((peaks[0].freq_hz - 1.05e9).abs() < 2.0e6);
    }
}
