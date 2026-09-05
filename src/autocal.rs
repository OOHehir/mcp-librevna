//! Automatic SOLT calibration driven by a LibreCAL module.
//!
//! This is the part where driving the module naively goes wrong. Setting the
//! module to SHORT and calling `VNA:CAL:MEASURE` produces a calibration, but
//! the GUI corrects it using whatever standard definitions its calibration kit
//! holds -- ideal ones, by default. The result looks entirely normal and is
//! quietly inaccurate, which is worse than an error.
//!
//! So the module's own factory coefficients are installed as a calibration kit
//! first, and every measurement is bound to the matching standard by name.
//! [`install_kit`] does that, [`calibrate_solt`] runs the sequence.
//!
//! Two indexing conventions collide here and neither is obvious:
//!
//! - **Calibration measurements are indexed from zero.** `VNA:CAL:ADD` appends,
//!   so the index of the new entry is the value `VNA:CAL:NUMBER?` returned
//!   *before* the add.
//! - **Calibration kit standards are indexed from one.** The GUI renames them
//!   to their position on every insertion, so an index is only valid until the
//!   next `NEW` or `DELete`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::Serialize;

use crate::calibration::SweepConfig;
use crate::error::{Result, VnaError};
use crate::instrument::Instrument;
use crate::librecal::{CalIdentity, LibreCal, Standard};
use crate::safety::Policy;

/// Where the coefficient files are written, relative to the working directory.
pub const STANDARDS_DIR: &str = "librecal-standards";

/// One module port and the VNA port it is cabled to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PortPair {
    pub cal_port: u8,
    pub vna_port: u8,
}

/// What `install_kit` put into the GUI, and what it displaced.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KitInstallation {
    pub standards: Vec<InstalledStandard>,
    /// Where the calibration kit that was already loaded got saved, if there
    /// was one. Installing the module's coefficients replaces the kit wholesale.
    pub replaced_kit: Option<String>,
}

/// A calibration standard installed into the GUI's kit.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InstalledStandard {
    /// Position in the kit, counting from one.
    pub index: usize,
    pub name: String,
    /// `Open`, `Short`, `Load` or `Through`, as the GUI names the type.
    pub kind: String,
    /// The coefficient entry it came from, e.g. `P1_OPEN`.
    pub coefficient: String,
    pub file: String,
    pub points: usize,
}

/// One measurement taken during the calibration.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CalStep {
    /// Position in the measurement list, counting from zero.
    pub entry: usize,
    pub standard: String,
    pub ports: Vec<u8>,
}

/// What a completed automatic calibration did.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AutoCalOutcome {
    pub cal_type: String,
    pub serial: String,
    pub coefficient_set: String,
    pub standards: Vec<InstalledStandard>,
    pub steps: Vec<CalStep>,
}

/// The reflection standards measured at every port, in the order used.
const REFLECT_KINDS: [(&str, Standard); 3] = [
    ("OPEN", Standard::Open),
    ("SHORT", Standard::Short),
    ("LOAD", Standard::Load),
];

/// Largest gap accepted between a corrected standard and its own definition.
///
/// Measured agreement on a healthy setup is within 0.06 dB across
/// 100 MHz - 6 GHz, so this leaves a wide margin while still being far tighter
/// than the gap a wrong kit produces.
pub const RESIDUAL_TOLERANCE_DB: f64 = 1.0;

/// One standard re-measured through the finished calibration.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ResidualCheck {
    pub standard: String,
    pub parameter: String,
    /// What the module's own coefficients say this standard is, averaged over
    /// the swept band.
    pub expected_db: f64,
    /// What the calibrated instrument actually reads.
    pub measured_db: f64,
    pub deviation_db: f64,
    pub ok: bool,
}

/// Frequency multiplier for a Touchstone option line's unit.
fn frequency_scale(unit: &str) -> Option<f64> {
    match unit.to_ascii_uppercase().as_str() {
        "HZ" => Some(1.0),
        "KHZ" => Some(1e3),
        "MHZ" => Some(1e6),
        "GHZ" => Some(1e9),
        _ => None,
    }
}

/// Mean magnitude of one S-parameter in a Touchstone file, over a band, in dB.
///
/// `column` is the index of the real part in a data row, counting the frequency
/// as column zero: 1 for a one-port file's S11, and 1/3/5/7 for S11/S21/S12/S22
/// in a two-port one.
///
/// The unit comes from the option line rather than being assumed. A file read
/// as Hz when it is written in GHz still parses, still interpolates, and is
/// wrong by nine orders of magnitude in a way no later check would notice.
pub fn touchstone_band_mean_db(
    text: &str,
    column: usize,
    start_hz: f64,
    stop_hz: f64,
) -> Option<f64> {
    let mut scale = 1e9;
    let mut sum = 0.0;
    let mut count = 0usize;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('!') {
            continue;
        }
        if let Some(options) = trimmed.strip_prefix('#') {
            if let Some(unit) = options.split_whitespace().next() {
                scale = frequency_scale(unit)?;
            }
            continue;
        }

        let values: Vec<f64> = trimmed
            .split_whitespace()
            .filter_map(|v| v.parse::<f64>().ok())
            .collect();
        if values.len() <= column + 1 {
            continue;
        }
        let freq = values[0] * scale;
        if freq < start_hz || freq > stop_hz {
            continue;
        }
        sum += values[column].hypot(values[column + 1]);
        count += 1;
    }

    if count == 0 {
        return None;
    }
    let mean = sum / count as f64;
    Some(if mean <= 0.0 {
        crate::scpi::parse::FLOOR_DB
    } else {
        20.0 * mean.log10()
    })
}

/// Name the kit standard built from one coefficient entry.
///
/// The module's own serial is part of the name so that a kit left in the GUI
/// cannot be mistaken for one built from a different module. No spaces: SCPI
/// splits parameters on them, so a name containing one would be truncated to
/// its first word without complaint.
fn standard_name(serial: &str, coefficient: &str) -> String {
    format!("LibreCAL_{serial}_{coefficient}")
}

/// Fetch one coefficient entry and write it as a Touchstone file.
///
/// The module already emits Touchstone, header and all, so the block is written
/// through unchanged rather than reformatted. Re-deriving it would only add a
/// way for the numbers reaching the calibration to differ from the numbers the
/// module actually holds.
async fn write_coefficients(
    cal: &mut LibreCal,
    set: &str,
    coefficient: &str,
    path: &Path,
) -> Result<usize> {
    let touchstone = cal.coefficients(set, coefficient).await?;
    let points = touchstone
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('!') && !t.starts_with('#')
        })
        .count();

    if points == 0 {
        return Err(VnaError::LibreCalRefused(format!(
            "coefficient set {set}/{coefficient} contained no data points"
        )));
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, touchstone).await?;
    Ok(points)
}

/// Install the module's factory coefficients as the GUI's calibration kit.
///
/// Every previously defined standard is removed first. A kit is a single
/// namespace shared with whatever the user had loaded before, and leaving
/// strangers in it invites a later measurement binding to the wrong one.
pub async fn install_kit(
    instrument: &mut Instrument,
    cal: &mut LibreCal,
    identity: &CalIdentity,
    set: &str,
    pairs: &[PortPair],
    dir: &Path,
) -> Result<KitInstallation> {
    let existing = instrument.query_usize("VNA:CAL:KIT:STA:NUMBER?").await?;

    // The GUI keeps no copy of the kit it is about to delete, so a commercial
    // kit would otherwise be lost to a tool that was run to calibrate.
    let replaced_kit = if existing > 0 {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = dir.join(format!("replaced-kit-{stamp}.calkit"));
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        instrument
            .set(&format!("VNA:CAL:KIT:SAVE {}", path.display()))
            .await?;
        Some(path.display().to_string())
    } else {
        None
    };

    // Empty the kit, always deleting the first entry: the GUI renumbers the
    // remainder on every removal, so any other index would go stale mid-loop.
    for _ in 0..existing {
        instrument.set("VNA:CAL:KIT:STA:DELete 1").await?;
    }

    instrument.set("VNA:CAL:KIT:MANufacturer LibreCAL").await?;
    instrument
        .set(&format!("VNA:CAL:KIT:SERial {}", identity.serial))
        .await?;
    instrument
        .set(&format!(
            "VNA:CAL:KIT:DESCription Factory_coefficients_read_from_LibreCAL_{}_firmware_{}",
            identity.serial, identity.firmware
        ))
        .await?;

    let mut installed = Vec::new();

    for pair in pairs {
        for (kind, _) in REFLECT_KINDS {
            let coefficient = format!("P{}_{kind}", pair.cal_port);
            let file = dir.join(format!("{coefficient}.s1p"));
            let points = write_coefficients(cal, set, &coefficient, &file).await?;

            // `Open` and `Short` are the GUI's spellings; the type name is
            // matched case-insensitively but the standard's own name is not.
            let type_name = match kind {
                "OPEN" => "Open",
                "SHORT" => "Short",
                _ => "Load",
            };
            let name = standard_name(&identity.serial, &coefficient);
            instrument
                .set(&format!("VNA:CAL:KIT:STA:NEW {type_name} {name}"))
                .await?;
            let index = instrument.query_usize("VNA:CAL:KIT:STA:NUMBER?").await?;

            // The trailing port argument only selects a column out of a multi-port
            // file, and these are genuine 1-port files.
            instrument
                .set(&format!("VNA:CAL:KIT:STA:{index}:FILE {}", file.display()))
                .await?;

            installed.push(InstalledStandard {
                index,
                name,
                kind: type_name.to_string(),
                coefficient,
                file: file.display().to_string(),
                points,
            });
        }
    }

    // A through needs two ports, so it only exists once both are cabled.
    if let Some(through) = through_pair(pairs) {
        let (lo, hi) = through;
        let coefficient = format!("P{lo}{hi}_THROUGH");
        let file = dir.join(format!("{coefficient}.s2p"));
        let points = write_coefficients(cal, set, &coefficient, &file).await?;

        let name = standard_name(&identity.serial, &coefficient);
        instrument
            .set(&format!("VNA:CAL:KIT:STA:NEW Through {name}"))
            .await?;
        let index = instrument.query_usize("VNA:CAL:KIT:STA:NUMBER?").await?;

        // FILE demands three parameters here, but the port arguments only pick
        // columns from a file of more than two ports, so both are inert.
        instrument
            .set(&format!(
                "VNA:CAL:KIT:STA:{index}:FILE {} 1 2",
                file.display()
            ))
            .await?;

        installed.push(InstalledStandard {
            index,
            name,
            kind: "Through".into(),
            coefficient,
            file: file.display().to_string(),
            points,
        });
    }

    Ok(KitInstallation {
        standards: installed,
        replaced_kit,
    })
}

/// The calibration type name for a set of ports.
///
/// `VNA:CAL:ACTIVATE` does not take a bare algorithm name: the types it accepts
/// are port-qualified, as `VNA:CAL:ACTIVATE?` reports -- `OSL_1`, `SOLT_12`,
/// `TRL_12` and so on. A bare `SOLT` is not among them, and the GUI answers an
/// unrecognised one with silence, so the calibration simply stays inactive
/// while every command appears to succeed.
///
/// Two ports give a full SOLT. One port has no through to measure, so the
/// honest name for open-short-load at a single port is OSL.
pub fn calibration_type(pairs: &[PortPair]) -> String {
    let mut ports: Vec<u8> = pairs.iter().map(|p| p.vna_port).collect();
    ports.sort_unstable();
    let suffix: String = ports.iter().map(|p| p.to_string()).collect();
    if ports.len() >= 2 {
        format!("SOLT_{suffix}")
    } else {
        format!("OSL_{suffix}")
    }
}

/// Activate `cal_type`, having checked the instrument offers it, and confirm it
/// actually became active.
///
/// Both halves matter. Asking for a type the GUI does not recognise is answered
/// with silence rather than an error, and even an accepted activation can leave
/// no calibration active if the measurements do not support the type -- in
/// which case every later reading is uncorrected while looking entirely normal.
pub async fn activate(instrument: &mut Instrument, cal_type: &str) -> Result<String> {
    let raw = instrument.query_string("VNA:CAL:ACTIVATE?").await?;
    let available: Vec<&str> = raw.split(',').map(str::trim).collect();
    if !available.iter().any(|t| t.eq_ignore_ascii_case(cal_type)) {
        return Err(VnaError::InvalidRequest(format!(
            "this device does not offer calibration type {cal_type:?}; it offers {}",
            available.join(", ")
        )));
    }

    instrument
        .set(&format!("VNA:CAL:ACTIVATE {cal_type}"))
        .await?;

    let active = instrument.query_string("VNA:CAL:ACTIVE?").await?;
    if active.eq_ignore_ascii_case("none") {
        return Err(VnaError::InvalidRequest(format!(
            "activating {cal_type} left no calibration active, so the measurements taken do \
             not support it. Readings would be uncorrected while appearing normal"
        )));
    }
    Ok(active)
}

/// The two module ports a through can be measured across, lowest first.
///
/// The firmware only names a through with its ports ascending: `P12_THROUGH`
/// exists and `P21_THROUGH` is refused.
fn through_pair(pairs: &[PortPair]) -> Option<(u8, u8)> {
    if pairs.len() != 2 {
        return None;
    }
    let (a, b) = (pairs[0].cal_port, pairs[1].cal_port);
    Some((a.min(b), a.max(b)))
}

/// Measure a full SOLT calibration, driving the module through each standard.
///
/// Assumes [`install_kit`] has just run: every measurement is bound by name to
/// the standard carrying that port's real coefficients, so nothing here falls
/// back on the GUI's ideal definitions.
pub async fn calibrate_solt(
    instrument: &mut Instrument,
    cal: &mut LibreCal,
    identity: &CalIdentity,
    pairs: &[PortPair],
    module_ports: u8,
    timeout: Duration,
) -> Result<Vec<CalStep>> {
    instrument.set("VNA:CAL:RESET").await?;

    let mut steps = Vec::new();

    for pair in pairs {
        for (kind, standard) in REFLECT_KINDS {
            // Only the port being measured is terminated; one left set on another
            // port would be measured by the next step as if it were fresh.
            cal.clear_all(module_ports).await?;
            cal.set_standard(pair.cal_port, standard).await?;

            let coefficient = format!("P{}_{kind}", pair.cal_port);
            let name = standard_name(&identity.serial, &coefficient);
            let entry = add_measurement(instrument, kind, &name).await?;
            instrument
                .set(&format!("VNA:CAL:PORT {entry} {}", pair.vna_port))
                .await?;
            measure(instrument, entry, timeout).await?;

            steps.push(CalStep {
                entry,
                standard: name,
                ports: vec![pair.vna_port],
            });
        }
    }

    if let Some((lo, hi)) = through_pair(pairs) {
        cal.clear_all(module_ports).await?;
        cal.set_standard(lo, Standard::Through(hi)).await?;

        let coefficient = format!("P{lo}{hi}_THROUGH");
        let name = standard_name(&identity.serial, &coefficient);
        let entry = add_measurement(instrument, "THROUGH", &name).await?;

        // The VNA ports are given in the same order as the module ports the
        // coefficients were measured across, so S21 is not transposed.
        let vna_lo = vna_port_for(pairs, lo)?;
        let vna_hi = vna_port_for(pairs, hi)?;
        instrument
            .set(&format!("VNA:CAL:PORT {entry} {vna_lo} {vna_hi}"))
            .await?;
        measure(instrument, entry, timeout).await?;

        steps.push(CalStep {
            entry,
            standard: name,
            ports: vec![vna_lo, vna_hi],
        });
    }

    cal.clear_all(module_ports).await?;
    Ok(steps)
}

/// Append a measurement bound to a named standard, returning its index.
async fn add_measurement(instrument: &mut Instrument, kind: &str, standard: &str) -> Result<usize> {
    // `VNA:CAL:ADD` appends, so the count taken beforehand is the new entry's
    // own zero-based index.
    let entry = instrument.query_usize("VNA:CAL:NUMBER?").await?;
    instrument
        .set(&format!("VNA:CAL:ADD {kind} {standard}"))
        .await?;

    // The GUI refuses a standard it cannot find, but it refuses by doing
    // nothing observable, so confirm the binding rather than assume it.
    let bound = instrument
        .query_string(&format!("VNA:CAL:STANDARD? {entry}"))
        .await?;
    if !bound.eq_ignore_ascii_case(standard) {
        return Err(VnaError::InvalidRequest(format!(
            "calibration entry {entry} bound to standard {bound:?} rather than {standard:?}; \
             the calibration kit does not hold the module's coefficients, so this \
             calibration would silently use ideal standards"
        )));
    }
    Ok(entry)
}

/// Run one calibration measurement and wait for it to actually be taken.
///
/// `VNA:CAL:MEASURE` does not measure anything by itself. It marks the entry
/// pending, and the *next* sweep fills it. So the sweep is driven here and
/// waited for, and only then may the caller change what is connected.
///
/// `VNA:CAL:BUSY?` is not a usable handshake and is deliberately not consulted.
/// The command exists, but on a v1.6.5 GUI it never reads true -- sampled every
/// 40 ms across a measurement it stays `FALSE` throughout, whether the
/// acquisition is running or stopped. Polling it returns instantly, which reads
/// as "finished" and lets the caller move the module to the next standard while
/// the pending measurement is still waiting for its sweep. The calibration then
/// completes, activates, and is wrong by one standard, with nothing in the
/// numbers to say so.
async fn measure(instrument: &mut Instrument, entry: usize, timeout: Duration) -> Result<()> {
    instrument.set(&format!("VNA:CAL:MEASURE {entry}")).await?;
    instrument.run_sweep(timeout).await
}

/// Re-measure the standards through the finished calibration and compare each
/// against its own definition.
///
/// This is what separates a calibration that completed from one that is right.
/// A corrected standard should read back as the coefficients describe it -- not
/// as zero -- so the load tracking its real reflection is the evidence that the
/// module's coefficients were used. Had the kit failed to load, the GUI would
/// have fallen back on ideal definitions and corrected the load towards a
/// perfect match instead, which this comparison catches immediately.
///
/// It is a self-consistency check rather than an independent one: these are the
/// same standards the calibration was solved from. It cannot prove the module's
/// factory data is accurate, only that the calibration is using it.
pub async fn verify_residuals(
    instrument: &mut Instrument,
    cal: &mut LibreCal,
    standards: &[InstalledStandard],
    pairs: &[PortPair],
    module_ports: u8,
    sweep: &SweepConfig,
    timeout: Duration,
) -> Result<Vec<ResidualCheck>> {
    let mut checks = Vec::new();

    // Fail rather than skip: "did they all pass?" is true of an empty list, so a
    // skipped check reports a calibration verified when nothing was.
    let expected = |coefficient: &str, column: usize| -> Result<(String, f64)> {
        let standard = standards
            .iter()
            .find(|s| s.coefficient == coefficient)
            .ok_or_else(|| {
                VnaError::InvalidRequest(format!(
                    "no calibration standard was installed for {coefficient}, so the \
                     calibration cannot be checked against it"
                ))
            })?;
        let text = std::fs::read_to_string(&standard.file).map_err(|e| {
            VnaError::InvalidRequest(format!(
                "could not re-read the coefficients at {} to check the calibration \
                 against them: {e}",
                standard.file
            ))
        })?;
        let db = touchstone_band_mean_db(&text, column, sweep.start_hz, sweep.stop_hz).ok_or_else(
            || {
                VnaError::InvalidRequest(format!(
                    "{} holds no coefficient data between {:.0} and {:.0} Hz, so there is \
                     nothing to check the calibration against over this sweep",
                    standard.file, sweep.start_hz, sweep.stop_hz
                ))
            },
        )?;
        Ok((standard.name.clone(), db))
    };

    for pair in pairs {
        cal.clear_all(module_ports).await?;
        cal.set_standard(pair.cal_port, Standard::Load).await?;
        instrument.run_sweep(timeout).await?;

        let parameter = format!("S{0}{0}", pair.vna_port);
        let measured = mean_trace_db(instrument, &parameter).await?;
        let coefficient = format!("P{}_LOAD", pair.cal_port);
        let (name, expected_db) = expected(&coefficient, 1)?;
        checks.push(build_check(name, parameter, expected_db, measured));
    }

    if let Some((lo, hi)) = through_pair(pairs) {
        cal.clear_all(module_ports).await?;
        cal.set_standard(lo, Standard::Through(hi)).await?;
        instrument.run_sweep(timeout).await?;

        let vna_lo = vna_port_for(pairs, lo)?;
        let vna_hi = vna_port_for(pairs, hi)?;
        let parameter = format!("S{vna_hi}{vna_lo}");
        let measured = mean_trace_db(instrument, &parameter).await?;
        let coefficient = format!("P{lo}{hi}_THROUGH");
        // Column 3 is S21's real part. A through is reciprocal, so this cannot
        // detect a transposed port order; construction is what enforces that.
        let (name, expected_db) = expected(&coefficient, 3)?;
        checks.push(build_check(name, parameter, expected_db, measured));
    }

    cal.clear_all(module_ports).await?;
    Ok(checks)
}

fn build_check(
    standard: String,
    parameter: String,
    expected_db: f64,
    measured_db: f64,
) -> ResidualCheck {
    let deviation_db = measured_db - expected_db;
    ResidualCheck {
        standard,
        parameter,
        expected_db,
        measured_db,
        deviation_db,
        ok: deviation_db.abs() <= RESIDUAL_TOLERANCE_DB,
    }
}

async fn mean_trace_db(instrument: &mut Instrument, trace: &str) -> Result<f64> {
    let points = instrument.read_vna_trace(trace).await?;
    crate::analysis::mean_magnitude_db(&points)
        .ok_or_else(|| VnaError::InvalidRequest(format!("trace {trace} returned no points")))
}

fn vna_port_for(pairs: &[PortPair], cal_port: u8) -> Result<u8> {
    pairs
        .iter()
        .find(|p| p.cal_port == cal_port)
        .map(|p| p.vna_port)
        .ok_or_else(|| {
            VnaError::InvalidRequest(format!(
                "module port {cal_port} is not mapped to a VNA port"
            ))
        })
}

/// Where the coefficient files for `serial` belong, inside the sandbox.
///
/// The serial is read from the module over USB, so it is untrusted input being
/// used to build a path. Resolving it through the policy rather than joining it
/// directly is what stops a serial containing `..` from placing files outside
/// the working directory, and keeps this consistent with every other tool that
/// writes a file.
pub fn standards_dir(
    policy: &Policy,
    serial: &str,
) -> std::result::Result<PathBuf, crate::error::SafetyError> {
    policy.resolve_path(&format!("{STANDARDS_DIR}/{serial}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs() -> Vec<PortPair> {
        vec![
            PortPair {
                cal_port: 1,
                vna_port: 1,
            },
            PortPair {
                cal_port: 2,
                vna_port: 2,
            },
        ]
    }

    #[test]
    fn a_through_needs_both_ports() {
        assert_eq!(through_pair(&pairs()), Some((1, 2)));
        assert_eq!(through_pair(&pairs()[..1]), None);
        assert_eq!(through_pair(&[]), None);
    }

    #[test]
    fn through_ports_are_ordered_for_the_firmware() {
        // The module only names a through with its ports ascending, so a
        // reversed mapping must still ask for P12_THROUGH.
        let reversed = vec![
            PortPair {
                cal_port: 2,
                vna_port: 1,
            },
            PortPair {
                cal_port: 1,
                vna_port: 2,
            },
        ];
        assert_eq!(through_pair(&reversed), Some((1, 2)));
    }

    #[test]
    fn a_reversed_mapping_keeps_the_vna_ports_with_their_module_ports() {
        // Cal 2 is on VNA 1 here, so the through must be measured VNA 2 then VNA 1
        // to match the coefficients, or S21 arrives transposed.
        let reversed = vec![
            PortPair {
                cal_port: 2,
                vna_port: 1,
            },
            PortPair {
                cal_port: 1,
                vna_port: 2,
            },
        ];
        let (lo, hi) = through_pair(&reversed).unwrap();
        assert_eq!(vna_port_for(&reversed, lo).unwrap(), 2);
        assert_eq!(vna_port_for(&reversed, hi).unwrap(), 1);
    }

    #[test]
    fn the_calibration_type_is_port_qualified() {
        // The device offers OSL_1/2/12, SOLT_1/2/12, ThroughNormalization_12 and
        // TRL_12. A bare "SOLT" is not among them and is silently ignored.
        assert_eq!(calibration_type(&pairs()), "SOLT_12");
    }

    #[test]
    fn one_port_calibrates_as_osl_since_there_is_no_through() {
        let single = vec![PortPair {
            cal_port: 2,
            vna_port: 2,
        }];
        assert_eq!(calibration_type(&single), "OSL_2");
    }

    #[test]
    fn the_type_names_ports_in_ascending_order() {
        let reversed = vec![
            PortPair {
                cal_port: 1,
                vna_port: 2,
            },
            PortPair {
                cal_port: 2,
                vna_port: 1,
            },
        ];
        assert_eq!(calibration_type(&reversed), "SOLT_12");
    }

    #[test]
    fn standard_names_carry_the_module_serial_and_have_no_spaces() {
        let name = standard_name("5mV8QRceRywA", "P1_OPEN");
        assert_eq!(name, "LibreCAL_5mV8QRceRywA_P1_OPEN");
        assert!(!name.contains(' '));
    }

    const LOAD_S1P: &str = "\
! Automatically created by LibreCAL firmware
# GHz S RI R 50.0
0.000009 0.993775 0.005197
1.000000 0.100000 0.000000
2.000000 0.100000 0.000000
8.500000 0.053985 -0.648592
";

    #[test]
    fn a_touchstone_band_mean_uses_only_the_swept_range() {
        // Only the two 1 GHz and 2 GHz rows fall inside, each |S| = 0.1.
        let db = touchstone_band_mean_db(LOAD_S1P, 1, 0.5e9, 6.0e9).unwrap();
        assert!((db - (-20.0)).abs() < 1e-9, "got {db}");
    }

    #[test]
    fn the_frequency_unit_comes_from_the_file() {
        // Read as Hz rather than GHz, every row would fall below the band and
        // the mean would be empty rather than quietly wrong.
        let as_hz = LOAD_S1P.replace("# GHz", "# Hz");
        assert!(touchstone_band_mean_db(&as_hz, 1, 0.5e9, 6.0e9).is_none());
    }

    #[test]
    fn an_unknown_frequency_unit_is_refused() {
        let bad = LOAD_S1P.replace("# GHz", "# furlongs");
        assert!(touchstone_band_mean_db(&bad, 1, 0.5e9, 6.0e9).is_none());
    }

    #[test]
    fn a_residual_matching_the_definition_passes() {
        // Measured on hardware: the load's own coefficients average -19.66 dB
        // over 100 MHz - 6 GHz and the calibrated instrument read -19.60 dB.
        let c = build_check("load".into(), "S11".into(), -19.66, -19.60);
        assert!(c.ok, "deviation {} dB", c.deviation_db);
    }

    #[test]
    fn an_ideal_standard_fallback_is_caught() {
        // Had the kit failed to load, the GUI would correct the load towards a
        // perfect match instead of tracking its real reflection.
        let c = build_check("load".into(), "S11".into(), -19.66, -40.0);
        assert!(!c.ok);
    }

    #[test]
    fn coefficient_files_stay_inside_the_working_directory() {
        let workdir = std::env::temp_dir().join(format!("autocal-sandbox-{}", std::process::id()));
        std::fs::create_dir_all(&workdir).unwrap();
        let policy = Policy::new(&workdir).unwrap();

        let ok = standards_dir(&policy, "5mV8QRceRywA").unwrap();
        assert!(ok.starts_with(policy.workdir()));

        // The serial is read from the module over USB. A hostile one must be
        // refused or confined, never land outside the working directory.
        for serial in [
            "../../etc",
            "/etc/cron.d",
            "..",
            "../..",
            "a/../../../b",
            "",
        ] {
            match standards_dir(&policy, serial) {
                Err(_) => {}
                Ok(path) => assert!(
                    path.starts_with(policy.workdir()),
                    "serial {serial:?} resolved to {path:?}, outside the working directory"
                ),
            }
        }

        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn an_unmapped_port_is_refused_rather_than_guessed() {
        assert!(vna_port_for(&pairs(), 3).is_err());
    }
}
