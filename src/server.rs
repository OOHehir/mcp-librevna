//! The MCP tool surface.
//!
//! Tools are deliberately thin: they gate on policy, validate against the
//! device's reported limits, delegate to [`crate::instrument`], and shape the
//! answer into something an agent can reason about without drowning in points.
//!
//! Two conventions run through every measurement result:
//!
//! - the device's live error flags travel with the numbers, so clipped or
//!   unlocked readings cannot be mistaken for good ones;
//! - the calibration verdict travels with them too, so interpolated data is
//!   never reported as calibrated.

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::analysis;
use crate::calibration::{CalValidity, SweepConfig};
use crate::device::{DeviceLimits, Mode, StatusFlags};
use crate::error::{Tier, VnaError};
use crate::instrument::Instrument;
use crate::safety::Policy;
use crate::scpi::parse::{self, Identity};

/// Commands never exposed, whatever tiers are enabled.
///
/// Firmware flashing can brick the unit and has no place in an agent's reach;
/// there is no upside to gating it rather than removing it.
const PERMANENTLY_DENIED: &[&str] = &["DEV:UPDATE"];

/// How many buckets a decimated trace is reduced to.
const DEFAULT_ENVELOPE_BUCKETS: usize = 96;

/// Connection and timing settings for the server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// `host:port` of the LibreVNA-GUI SCPI server.
    pub addr: String,
    /// Budget for an ordinary SCPI exchange.
    pub timeout: Duration,
    /// Budget for a sweep to complete, including averaging.
    pub sweep_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            addr: format!("127.0.0.1:{}", crate::scpi::client::DEFAULT_SCPI_PORT),
            timeout: Duration::from_secs(10),
            sweep_timeout: Duration::from_secs(120),
        }
    }
}

/// The MCP server.
#[derive(Clone)]
pub struct LibreVnaServer {
    config: ServerConfig,
    policy: Arc<Policy>,
    instrument: Arc<Mutex<Option<Instrument>>>,
}

fn mcp_err(e: VnaError) -> ErrorData {
    match &e {
        // A refused request is the agent's mistake to correct, not a fault.
        VnaError::Safety(_) => ErrorData::invalid_params(e.to_string(), None),
        _ => ErrorData::internal_error(e.to_string(), None),
    }
}

// ---------------------------------------------------------------- arguments

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConnectArgs {
    /// Serial number of the unit to use. Omit to take the first one found.
    #[serde(default)]
    pub serial: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConfigureSweepArgs {
    /// Start frequency in Hz. Give either start/stop or center/span.
    #[serde(default)]
    pub start_hz: Option<f64>,
    /// Stop frequency in Hz.
    #[serde(default)]
    pub stop_hz: Option<f64>,
    /// Centre frequency in Hz.
    #[serde(default)]
    pub center_hz: Option<f64>,
    /// Span in Hz.
    #[serde(default)]
    pub span_hz: Option<f64>,
    /// Points per sweep. More points means finer resolution and a slower sweep.
    #[serde(default)]
    pub points: Option<usize>,
    /// IF bandwidth in Hz. Lower widens dynamic range and slows the sweep.
    #[serde(default)]
    pub ifbw_hz: Option<f64>,
    /// Stimulus power in dBm. Subject to the configured ceiling.
    #[serde(default)]
    pub power_dbm: Option<f64>,
    /// Sweeps to average. 1 disables averaging.
    #[serde(default)]
    pub averaging: Option<usize>,
    /// Logarithmic frequency spacing instead of linear.
    #[serde(default)]
    pub logarithmic: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SweepArgs {
    /// Seconds to wait for the sweep, including averaging.
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    /// Trace to read, e.g. "S11". Defaults to the first defined trace.
    #[serde(default)]
    pub trace: Option<String>,
    /// Envelope buckets in the returned curve. Full data goes to Touchstone.
    #[serde(default)]
    pub envelope_points: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MarkerArgs {
    /// Trace to sample.
    pub trace: String,
    /// Frequency of interest, in Hz. Must lie within the swept range.
    pub freq_hz: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnalyzeArgs {
    /// Trace to analyse. Defaults to the first defined trace.
    #[serde(default)]
    pub trace: Option<String>,
    /// Depth below the minimum at which bandwidth is measured, in dB.
    #[serde(default)]
    pub depth_db: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportArgs {
    /// Traces to export, in Touchstone order, e.g. ["S11","S12","S21","S22"].
    pub traces: Vec<String>,
    /// Filename inside the working directory, e.g. "filter.s2p".
    pub filename: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceManageArgs {
    /// What to do: create, delete, rename, pause, resume, or set_parameter.
    pub action: String,
    /// Trace the action applies to.
    pub trace: String,
    /// New name for rename, or the S-parameter for set_parameter.
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalMeasureArgs {
    /// Standard to measure: OPEN, SHORT, LOAD, THROUGH, ISOLATION, SLIDINGLOAD,
    /// REFLECT or LINE.
    pub standard: String,
    /// Port the standard is attached to. Omit for two-port standards.
    #[serde(default)]
    pub port: Option<u8>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalActivateArgs {
    /// Calibration type, exactly as the device names it, e.g. "SOLT_12" or
    /// "OSL_1". Read available_types from vna_cal_status first.
    pub cal_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalFileArgs {
    /// Filename inside the working directory.
    pub filename: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SaConfigureArgs {
    #[serde(default)]
    pub start_hz: Option<f64>,
    #[serde(default)]
    pub stop_hz: Option<f64>,
    #[serde(default)]
    pub center_hz: Option<f64>,
    #[serde(default)]
    pub span_hz: Option<f64>,
    /// Resolution bandwidth in Hz.
    #[serde(default)]
    pub rbw_hz: Option<f64>,
    /// Window: NONE, KAISER, HANN or FLATTOP.
    #[serde(default)]
    pub window: Option<String>,
    /// Detector: +PEAK, -PEAK, NORMAL, SAMPLE or AVERAGE.
    #[serde(default)]
    pub detector: Option<String>,
    /// Sweeps to average.
    #[serde(default)]
    pub averaging: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SaReadArgs {
    /// Trace to read. Defaults to the first defined trace.
    #[serde(default)]
    pub trace: Option<String>,
    /// Only report peaks at or above this level, in dBm.
    #[serde(default)]
    pub floor_dbm: Option<f64>,
    /// Maximum number of peaks to report.
    #[serde(default)]
    pub max_peaks: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenArgs {
    /// Output frequency in Hz.
    pub freq_hz: f64,
    /// Output power in dBm. Subject to the configured ceiling.
    pub power_dbm: f64,
    /// Port to drive: 1 or 2.
    pub port: u8,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RawArgs {
    /// The SCPI command to send verbatim. A trailing `?` on the verb makes it a
    /// query and the response is returned.
    pub command: String,
}

// ------------------------------------------------------------------ results

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConnectResult {
    pub identity: Identity,
    pub limits: DeviceLimits,
    pub mode: Mode,
    pub calibration: CalValidity,
    /// The stimulus ceiling in force, in dBm.
    pub max_stimulus_dbm: f64,
    /// Capability tiers enabled for this session.
    pub enabled_tiers: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusResult {
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sweep: Option<SweepConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration: Option<CalValidity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<StatusFlags>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperatures: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub enabled_tiers: Vec<String>,
    pub max_stimulus_dbm: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SweepResult {
    pub sweep: SweepConfig,
    pub flags: StatusFlags,
    pub calibration: CalValidity,
    /// Everything the agent should know before trusting these numbers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TraceReadResult {
    pub trace: String,
    pub points: usize,
    pub sweep: SweepConfig,
    pub minimum: Option<analysis::Extreme>,
    pub maximum: Option<analysis::Extreme>,
    /// Min/max envelope of the full trace, so narrow features survive.
    pub envelope: Vec<analysis::EnvelopeBin>,
    pub flags: StatusFlags,
    pub calibration: CalValidity,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MarkerResult {
    pub trace: String,
    pub freq_hz: f64,
    pub magnitude_db: f64,
    pub vswr: f64,
    pub calibration: CalValidity,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AnalyzeResult {
    pub trace: String,
    pub summary: analysis::ResonanceSummary,
    pub sweep: SweepConfig,
    pub flags: StatusFlags,
    pub calibration: CalValidity,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SaReadResult {
    pub trace: String,
    pub points: usize,
    pub peaks: Vec<analysis::Peak>,
    pub noise_floor_dbm: f64,
    pub flags: StatusFlags,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExportResult {
    pub path: String,
    pub bytes: usize,
    pub traces: Vec<String>,
    pub calibration: CalValidity,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Acknowledged {
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CalStatusResult {
    pub active: Option<String>,
    pub validity: CalValidity,
    pub available_types: Vec<String>,
    pub measurements: usize,
    pub busy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RawResult {
    pub command: String,
    pub response: Option<String>,
}

// ------------------------------------------------------------------- server

impl LibreVnaServer {
    pub fn new(config: ServerConfig, policy: Policy) -> Self {
        Self {
            config,
            policy: Arc::new(policy),
            instrument: Arc::new(Mutex::new(None)),
        }
    }

    fn enabled_tiers(&self) -> Vec<String> {
        [
            Tier::ReadOnly,
            Tier::Measure,
            Tier::Emission,
            Tier::Destructive,
            Tier::ManualHardware,
        ]
        .into_iter()
        .filter(|t| self.policy.allows(*t))
        .map(|t| t.to_string())
        .collect()
    }

    /// Collect every reason the caller should hesitate over a result.
    fn warnings(flags: &StatusFlags, calibration: &CalValidity) -> Vec<String> {
        [flags.warning(), calibration.warning()]
            .into_iter()
            .flatten()
            .collect()
    }
}

#[tool_router]
impl LibreVnaServer {
    #[tool(
        description = "Connect to a LibreVNA through LibreVNA-GUI. Returns the unit's \
                       identity, the frequency/power/point limits it reports for itself, \
                       and which capability tiers this server was started with. Call this \
                       before any other tool."
    )]
    pub async fn librevna_connect(
        &self,
        Parameters(args): Parameters<ConnectArgs>,
    ) -> Result<Json<ConnectResult>, ErrorData> {
        let mut instrument = Instrument::connect(
            &self.config.addr,
            self.config.timeout,
            args.serial.as_deref(),
        )
        .await
        .map_err(mcp_err)?;

        let mode = instrument.mode().await.map_err(mcp_err)?;
        let calibration = instrument.calibration_validity().await.map_err(mcp_err)?;
        let result = ConnectResult {
            identity: instrument.identity().clone(),
            limits: *instrument.limits(),
            mode,
            calibration,
            max_stimulus_dbm: self.policy.max_stimulus_dbm(),
            enabled_tiers: self.enabled_tiers(),
        };

        *self.instrument.lock().await = Some(instrument);
        Ok(Json(result))
    }

    #[tool(description = "Release the LibreVNA, leaving LibreVNA-GUI running.")]
    pub async fn librevna_disconnect(&self) -> Result<Json<Acknowledged>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        if let Some(instrument) = guard.as_mut() {
            instrument.disconnect().await.map_err(mcp_err)?;
        }
        *guard = None;
        Ok(Json(Acknowledged {
            ok: true,
            detail: "Disconnected from the LibreVNA.".into(),
        }))
    }

    #[tool(
        description = "Report connection state, mode, current sweep, calibration validity, \
                       device error flags and temperatures. Safe to call at any time."
    )]
    pub async fn librevna_status(&self) -> Result<Json<StatusResult>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let Some(instrument) = guard.as_mut() else {
            return Ok(Json(StatusResult {
                connected: false,
                serial: None,
                mode: None,
                sweep: None,
                calibration: None,
                flags: None,
                temperatures: None,
                warning: Some("Not connected. Call librevna_connect first.".into()),
                enabled_tiers: self.enabled_tiers(),
                max_stimulus_dbm: self.policy.max_stimulus_dbm(),
            }));
        };

        let serial = instrument.connected_serial().await.map_err(mcp_err)?;
        let mode = instrument.mode().await.map_err(mcp_err)?;
        let sweep = instrument.current_sweep().await.map_err(mcp_err)?;
        let flags = instrument.status().await.map_err(mcp_err)?;
        let calibration = instrument.calibration_validity().await.map_err(mcp_err)?;
        let temperatures = instrument.temperatures().await.ok();
        let warnings = Self::warnings(&flags, &calibration);

        Ok(Json(StatusResult {
            connected: true,
            serial: Some(serial),
            mode: Some(mode),
            sweep: Some(sweep),
            calibration: Some(calibration),
            flags: Some(flags),
            temperatures,
            warning: (!warnings.is_empty()).then(|| warnings.join(" ")),
            enabled_tiers: self.enabled_tiers(),
            max_stimulus_dbm: self.policy.max_stimulus_dbm(),
        }))
    }

    #[tool(
        description = "Stop all RF output immediately: disable the generator, the tracking \
                       generator and any running sweep. Always available regardless of which \
                       capability tiers are enabled."
    )]
    pub async fn librevna_rf_off(&self) -> Result<Json<Acknowledged>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;
        instrument.all_rf_off().await.map_err(mcp_err)?;
        Ok(Json(Acknowledged {
            ok: true,
            detail: "Generator off, tracking generator off, acquisition stopped.".into(),
        }))
    }

    #[tool(description = "List the serial numbers of every LibreVNA that LibreVNA-GUI can see.")]
    pub async fn librevna_list_devices(&self) -> Result<Json<Vec<String>>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;
        let raw = instrument
            .query_string("DEV:LIST?")
            .await
            .map_err(mcp_err)?;
        Ok(Json(crate::scpi::parse::parse_list(&raw)))
    }

    #[tool(
        description = "Configure the VNA sweep. Give start_hz/stop_hz or center_hz/span_hz. \
                       Every value is checked against the limits this device reports before \
                       anything is sent. Changing the span or point count after calibrating \
                       will downgrade the calibration to interpolated -- the response says so."
    )]
    pub async fn vna_configure_sweep(
        &self,
        Parameters(args): Parameters<ConfigureSweepArgs>,
    ) -> Result<Json<SweepResult>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;
        let limits = *instrument.limits();

        // Resolve centre/span into start/stop so one code path validates both.
        let (start, stop) = match (args.start_hz, args.stop_hz, args.center_hz, args.span_hz) {
            (Some(a), Some(b), _, _) => (Some(a), Some(b)),
            (_, _, Some(c), Some(s)) => (Some(c - s / 2.0), Some(c + s / 2.0)),
            (a, b, _, _) => (a, b),
        };

        // Validate everything before transmitting anything, so a rejected
        // request cannot leave the sweep half-reconfigured.
        if let (Some(a), Some(b)) = (start, stop) {
            limits.check_span(a, b).map_err(|e| mcp_err(e.into()))?;
        } else {
            if let Some(a) = start {
                limits
                    .check_frequency("start frequency", a)
                    .map_err(|e| mcp_err(e.into()))?;
            }
            if let Some(b) = stop {
                limits
                    .check_frequency("stop frequency", b)
                    .map_err(|e| mcp_err(e.into()))?;
            }
        }
        if let Some(points) = args.points {
            limits.check_points(points).map_err(|e| mcp_err(e.into()))?;
        }
        if let Some(ifbw) = args.ifbw_hz {
            limits.check_ifbw(ifbw).map_err(|e| mcp_err(e.into()))?;
        }
        if let Some(power) = args.power_dbm {
            self.policy
                .check_stimulus(power, &limits)
                .map_err(|e| mcp_err(e.into()))?;
        }

        instrument.set_mode(Mode::Vna).await.map_err(mcp_err)?;

        // Order matters: widen before narrowing so an intermediate state never
        // has stop below start.
        if let (Some(a), Some(b)) = (start, stop) {
            let current = instrument.current_sweep().await.map_err(mcp_err)?;
            if a > current.stop_hz {
                instrument
                    .set(&format!("VNA:FREQ:STOP {b}"))
                    .await
                    .map_err(mcp_err)?;
                instrument
                    .set(&format!("VNA:FREQ:START {a}"))
                    .await
                    .map_err(mcp_err)?;
            } else {
                instrument
                    .set(&format!("VNA:FREQ:START {a}"))
                    .await
                    .map_err(mcp_err)?;
                instrument
                    .set(&format!("VNA:FREQ:STOP {b}"))
                    .await
                    .map_err(mcp_err)?;
            }
        } else {
            if let Some(a) = start {
                instrument
                    .set(&format!("VNA:FREQ:START {a}"))
                    .await
                    .map_err(mcp_err)?;
            }
            if let Some(b) = stop {
                instrument
                    .set(&format!("VNA:FREQ:STOP {b}"))
                    .await
                    .map_err(mcp_err)?;
            }
        }

        if let Some(points) = args.points {
            instrument
                .set(&format!("VNA:ACQ:POINTS {points}"))
                .await
                .map_err(mcp_err)?;
        }
        if let Some(ifbw) = args.ifbw_hz {
            instrument
                .set(&format!("VNA:ACQ:IFBW {ifbw}"))
                .await
                .map_err(mcp_err)?;
        }
        if let Some(power) = args.power_dbm {
            instrument
                .set(&format!("VNA:STIM:LVL {power}"))
                .await
                .map_err(mcp_err)?;
        }
        if let Some(averaging) = args.averaging {
            instrument
                .set(&format!("VNA:ACQ:AVG {averaging}"))
                .await
                .map_err(mcp_err)?;
        }
        if let Some(log) = args.logarithmic {
            let kind = if log { "LOG" } else { "LIN" };
            instrument
                .set(&format!("VNA:SWEEPTYPE {kind}"))
                .await
                .map_err(mcp_err)?;
        }

        let sweep = instrument.current_sweep().await.map_err(mcp_err)?;
        let flags = instrument.status().await.map_err(mcp_err)?;
        let calibration = instrument.calibration_validity().await.map_err(mcp_err)?;
        Ok(Json(SweepResult {
            warnings: Self::warnings(&flags, &calibration),
            sweep,
            flags,
            calibration,
        }))
    }

    #[tool(
        description = "Run one VNA sweep and wait for it to finish, including any configured \
                       averaging. Returns the device error flags and calibration validity; \
                       read the trace afterwards with vna_read or vna_analyze."
    )]
    pub async fn vna_sweep(
        &self,
        Parameters(args): Parameters<SweepArgs>,
    ) -> Result<Json<SweepResult>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;

        let timeout = args
            .timeout_s
            .map(Duration::from_secs)
            .unwrap_or(self.config.sweep_timeout);

        instrument.set_mode(Mode::Vna).await.map_err(mcp_err)?;
        instrument.run_sweep(timeout).await.map_err(mcp_err)?;

        let sweep = instrument.current_sweep().await.map_err(mcp_err)?;
        let flags = instrument.status().await.map_err(mcp_err)?;
        let calibration = instrument.calibration_validity().await.map_err(mcp_err)?;
        Ok(Json(SweepResult {
            warnings: Self::warnings(&flags, &calibration),
            sweep,
            flags,
            calibration,
        }))
    }

    #[tool(
        description = "Read a VNA trace as a summary: its minimum, maximum, and a min/max \
                       envelope of the whole sweep. The envelope preserves narrow notches that \
                       plain subsampling would step over. For the full data, use \
                       vna_export_touchstone."
    )]
    pub async fn vna_read(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<Json<TraceReadResult>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;

        let trace = resolve_trace(instrument, args.trace, TraceKind::Vna).await?;
        let points = instrument.read_vna_trace(&trace).await.map_err(mcp_err)?;
        let sweep = instrument.current_sweep().await.map_err(mcp_err)?;
        let flags = instrument.status().await.map_err(mcp_err)?;
        let calibration = instrument.calibration_validity().await.map_err(mcp_err)?;

        let buckets = args.envelope_points.unwrap_or(DEFAULT_ENVELOPE_BUCKETS);
        Ok(Json(TraceReadResult {
            trace,
            points: points.len(),
            sweep,
            minimum: analysis::find_minimum(&points),
            maximum: analysis::find_maximum(&points),
            envelope: analysis::envelope_decimate(&points, buckets),
            warnings: Self::warnings(&flags, &calibration),
            flags,
            calibration,
        }))
    }

    #[tool(
        description = "Read one trace value at a specific frequency, interpolating between \
                       swept points. Refuses frequencies outside the swept range rather than \
                       extrapolating."
    )]
    pub async fn vna_marker(
        &self,
        Parameters(args): Parameters<MarkerArgs>,
    ) -> Result<Json<MarkerResult>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;

        let points = instrument
            .read_vna_trace(&args.trace)
            .await
            .map_err(mcp_err)?;
        let magnitude_db = analysis::magnitude_at(&points, args.freq_hz).ok_or_else(|| {
            ErrorData::invalid_params(
                format!(
                    "{} Hz is outside the swept range; widen the sweep or pick a frequency \
                     inside it.",
                    args.freq_hz
                ),
                None,
            )
        })?;

        let flags = instrument.status().await.map_err(mcp_err)?;
        let calibration = instrument.calibration_validity().await.map_err(mcp_err)?;
        Ok(Json(MarkerResult {
            trace: args.trace,
            freq_hz: args.freq_hz,
            magnitude_db,
            vswr: analysis::vswr_from_db(magnitude_db),
            warnings: Self::warnings(&flags, &calibration),
            calibration,
        }))
    }

    #[tool(
        description = "Summarise a reflection trace: resonant frequency, return loss, VSWR, \
                       bandwidth and loaded Q. Bandwidth is measured where the response rises \
                       depth_db above its minimum, and is omitted if the sweep is too narrow \
                       to resolve it."
    )]
    pub async fn vna_analyze(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<Json<AnalyzeResult>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;

        let trace = resolve_trace(instrument, args.trace, TraceKind::Vna).await?;
        let points = instrument.read_vna_trace(&trace).await.map_err(mcp_err)?;
        let depth_db = args.depth_db.unwrap_or(3.0);

        let summary = analysis::summarise_resonance(&points, depth_db).ok_or_else(|| {
            ErrorData::internal_error(format!("trace {trace} returned no points to analyse"), None)
        })?;

        let sweep = instrument.current_sweep().await.map_err(mcp_err)?;
        let flags = instrument.status().await.map_err(mcp_err)?;
        let calibration = instrument.calibration_validity().await.map_err(mcp_err)?;

        // An under-resolved feature is as misleading as an uncalibrated one, so
        // it travels in the same place the agent already reads.
        let mut warnings = Self::warnings(&flags, &calibration);
        warnings.extend(summary.warning());

        Ok(Json(AnalyzeResult {
            trace,
            summary,
            sweep,
            warnings,
            flags,
            calibration,
        }))
    }

    #[tool(description = "List the traces defined in VNA mode.")]
    pub async fn vna_trace_list(&self) -> Result<Json<Vec<String>>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;
        Ok(Json(instrument.vna_traces().await.map_err(mcp_err)?))
    }

    #[tool(
        description = "Create, delete, rename, pause, resume a trace, or set its S-parameter. \
                       action is one of: create, delete, rename, pause, resume, set_parameter."
    )]
    pub async fn vna_trace_manage(
        &self,
        Parameters(args): Parameters<TraceManageArgs>,
    ) -> Result<Json<Acknowledged>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;

        let trace = &args.trace;
        let command = match args.action.to_ascii_lowercase().as_str() {
            "create" => format!("VNA:TRAC:NEW {trace}"),
            // Deleting a trace discards captured data, so it needs the tier.
            "delete" => {
                self.policy
                    .require(Tier::Destructive, "vna_trace_manage(delete)")
                    .map_err(|e| mcp_err(e.into()))?;
                format!("VNA:TRAC:DELETE {trace}")
            }
            "pause" => format!("VNA:TRAC:PAUSE {trace}"),
            "resume" => format!("VNA:TRAC:RESUME {trace}"),
            "rename" => {
                let name = require_value(&args.value, "rename needs `value` as the new name")?;
                format!("VNA:TRAC:RENAME {trace} {name}")
            }
            "set_parameter" => {
                let param = require_value(&args.value, "set_parameter needs `value`, e.g. S21")?;
                format!("VNA:TRAC:PARAM {trace} {param}")
            }
            other => {
                return Err(ErrorData::invalid_params(
                    format!(
                        "unknown action {other:?}; expected create, delete, rename, pause, \
                         resume or set_parameter"
                    ),
                    None,
                ));
            }
        };

        instrument.set(&command).await.map_err(mcp_err)?;
        Ok(Json(Acknowledged {
            ok: true,
            detail: format!("Applied `{command}`."),
        }))
    }

    #[tool(
        description = "Write the current traces to a Touchstone file in the working directory. \
                       This is how to obtain full-resolution data; the read tools return \
                       summaries. Requires the destructive tier because it writes to disk."
    )]
    pub async fn vna_export_touchstone(
        &self,
        Parameters(args): Parameters<ExportArgs>,
    ) -> Result<Json<ExportResult>, ErrorData> {
        self.policy
            .require(Tier::Destructive, "vna_export_touchstone")
            .map_err(|e| mcp_err(e.into()))?;
        let path = self
            .policy
            .resolve_path(&args.filename)
            .map_err(|e| mcp_err(e.into()))?;

        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;

        let text = instrument.touchstone(&args.traces).await.map_err(mcp_err)?;

        // A zero-byte file reported as success would leave the agent believing
        // it had captured a measurement.
        if text.trim().is_empty() {
            return Err(ErrorData::internal_error(
                format!(
                    "the instrument returned no Touchstone data for {:?}; check the traces                      exist and a sweep has completed",
                    args.traces
                ),
                None,
            ));
        }

        let calibration = instrument.calibration_validity().await.map_err(mcp_err)?;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| mcp_err(VnaError::Transport(e)))?;
        }
        tokio::fs::write(&path, &text)
            .await
            .map_err(|e| mcp_err(VnaError::Transport(e)))?;

        Ok(Json(ExportResult {
            path: path.display().to_string(),
            bytes: text.len(),
            traces: args.traces,
            warnings: calibration.warning().into_iter().collect(),
            calibration,
        }))
    }

    #[tool(
        description = "Report the active calibration, which types this device offers, how many \
                       standards have been measured, and crucially whether the calibration \
                       still applies to the sweep now configured."
    )]
    pub async fn vna_cal_status(&self) -> Result<Json<CalStatusResult>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;

        instrument.refresh_calibration().await.map_err(mcp_err)?;
        let validity = instrument.calibration_validity().await.map_err(mcp_err)?;
        let available = instrument
            .query_string("VNA:CAL:ACTIVATE?")
            .await
            .map_err(mcp_err)?;
        let measurements = instrument.query_usize("VNA:CAL:NUMBER?").await.unwrap_or(0);
        let busy = instrument
            .query_bool("VNA:CAL:BUSY?")
            .await
            .unwrap_or(false);

        Ok(Json(CalStatusResult {
            active: instrument.calibration().active_type.clone(),
            warning: validity.warning(),
            validity,
            available_types: crate::scpi::parse::parse_list(&available),
            measurements,
            busy,
        }))
    }

    #[tool(
        description = "Measure one calibration standard. Attach the standard to the stated \
                       port first, then call this; it blocks until the measurement completes. \
                       standard is OPEN, SHORT, LOAD, THROUGH, ISOLATION, SLIDINGLOAD, REFLECT \
                       or LINE."
    )]
    pub async fn vna_cal_measure(
        &self,
        Parameters(args): Parameters<CalMeasureArgs>,
    ) -> Result<Json<Acknowledged>, ErrorData> {
        const KNOWN_STANDARDS: &[&str] = &[
            "OPEN",
            "SHORT",
            "LOAD",
            "THROUGH",
            "ISOLATION",
            "SLIDINGLOAD",
            "REFLECT",
            "LINE",
        ];

        let standard = args.standard.to_ascii_uppercase();
        if !KNOWN_STANDARDS.contains(&standard.as_str()) {
            return Err(ErrorData::invalid_params(
                format!(
                    "unknown standard {standard:?}; expected one of {}",
                    KNOWN_STANDARDS.join(", ")
                ),
                None,
            ));
        }

        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;

        let index = instrument
            .query_usize("VNA:CAL:NUMBER?")
            .await
            .map_err(mcp_err)?;
        instrument
            .set(&format!("VNA:CAL:ADD {standard}"))
            .await
            .map_err(mcp_err)?;
        if let Some(port) = args.port {
            instrument
                .set(&format!("VNA:CAL:PORT {index} {port}"))
                .await
                .map_err(mcp_err)?;
        }
        instrument
            .set(&format!("VNA:CAL:MEASURE {index}"))
            .await
            .map_err(mcp_err)?;

        // MEASURE only marks the entry pending; the next sweep fills it.
        // `VNA:CAL:BUSY?` never reads true on a v1.6.5 GUI, so it is no handshake.
        instrument
            .run_sweep(self.config.sweep_timeout)
            .await
            .map_err(mcp_err)?;

        Ok(Json(Acknowledged {
            ok: true,
            detail: format!(
                "Measured {standard} as calibration entry {index}. Leave the standard \
                 connected until this returns."
            ),
        }))
    }

    #[tool(
        description = "Activate a calibration type once its standards are measured, e.g. \
                       SOLT_12 for a full 2-port or OSL_1 for port 1 only. Names are \
                       device-specific: read available_types from vna_cal_status first."
    )]
    pub async fn vna_cal_activate(
        &self,
        Parameters(args): Parameters<CalActivateArgs>,
    ) -> Result<Json<CalStatusResult>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;

        // The instrument ignores an unknown type silently, and a set draws no
        // reply, so *OPC? still returns 1 and the activation looks like it
        // worked. Check the name against the device's own list first.
        let offered = instrument
            .query_string("VNA:CAL:ACTIVATE?")
            .await
            .map_err(mcp_err)?;
        let available = parse::parse_list(&offered);
        if !available
            .iter()
            .any(|t| t.eq_ignore_ascii_case(&args.cal_type))
        {
            return Err(ErrorData::invalid_params(
                format!(
                    "this device offers no calibration type {:?}. It offers: {}",
                    args.cal_type,
                    available.join(", ")
                ),
                None,
            ));
        }

        instrument
            .set(&format!("VNA:CAL:ACTIVATE {}", args.cal_type))
            .await
            .map_err(mcp_err)?;
        instrument.refresh_calibration().await.map_err(mcp_err)?;

        // A valid type still fails to activate if its standards are unmeasured,
        // and fails just as quietly.
        match instrument.calibration().active_type.as_deref() {
            Some(active) if active.eq_ignore_ascii_case(&args.cal_type) => {}
            other => {
                return Err(ErrorData::internal_error(
                    format!(
                        "the instrument did not activate {:?} (it reports {}). Measure every \
                         standard the type needs with vna_cal_measure first.",
                        args.cal_type,
                        other.unwrap_or("no calibration")
                    ),
                    None,
                ));
            }
        }

        let validity = instrument.calibration_validity().await.map_err(mcp_err)?;
        Ok(Json(CalStatusResult {
            active: instrument.calibration().active_type.clone(),
            warning: validity.warning(),
            validity,
            available_types: Vec::new(),
            measurements: instrument.query_usize("VNA:CAL:NUMBER?").await.unwrap_or(0),
            busy: false,
        }))
    }

    #[tool(description = "Save the active calibration to a file in the working directory.")]
    pub async fn vna_cal_save(
        &self,
        Parameters(args): Parameters<CalFileArgs>,
    ) -> Result<Json<Acknowledged>, ErrorData> {
        self.policy
            .require(Tier::Destructive, "vna_cal_save")
            .map_err(|e| mcp_err(e.into()))?;
        let path = self
            .policy
            .resolve_path(&args.filename)
            .map_err(|e| mcp_err(e.into()))?;

        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;
        instrument
            .set(&format!("VNA:CAL:SAVE {}", path.display()))
            .await
            .map_err(mcp_err)?;

        Ok(Json(Acknowledged {
            ok: true,
            detail: format!("Calibration saved to {}.", path.display()),
        }))
    }

    #[tool(
        description = "Load a calibration from a file in the working directory. The response \
                       states whether it applies to the sweep now configured."
    )]
    pub async fn vna_cal_load(
        &self,
        Parameters(args): Parameters<CalFileArgs>,
    ) -> Result<Json<CalStatusResult>, ErrorData> {
        let path = self
            .policy
            .resolve_path(&args.filename)
            .map_err(|e| mcp_err(e.into()))?;

        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;
        instrument
            .query_string(&format!("VNA:CAL:LOAD? {}", path.display()))
            .await
            .map_err(mcp_err)?;
        instrument.refresh_calibration().await.map_err(mcp_err)?;

        let validity = instrument.calibration_validity().await.map_err(mcp_err)?;
        Ok(Json(CalStatusResult {
            active: instrument.calibration().active_type.clone(),
            warning: validity.warning(),
            validity,
            available_types: Vec::new(),
            measurements: instrument.query_usize("VNA:CAL:NUMBER?").await.unwrap_or(0),
            busy: false,
        }))
    }

    #[tool(
        description = "Discard the active calibration and all measured standards. This cannot \
                       be undone; save the calibration first if it might be wanted again."
    )]
    pub async fn vna_cal_reset(&self) -> Result<Json<Acknowledged>, ErrorData> {
        self.policy
            .require(Tier::Destructive, "vna_cal_reset")
            .map_err(|e| mcp_err(e.into()))?;

        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;
        instrument.set("VNA:CAL:RESET").await.map_err(mcp_err)?;
        instrument.refresh_calibration().await.map_err(mcp_err)?;

        Ok(Json(Acknowledged {
            ok: true,
            detail: "Calibration reset. Measurements are now uncorrected.".into(),
        }))
    }

    #[tool(
        description = "Configure the spectrum analyser: span, resolution bandwidth, window, \
                       detector and averaging. Values are checked against this device's \
                       reported limits."
    )]
    pub async fn sa_configure(
        &self,
        Parameters(args): Parameters<SaConfigureArgs>,
    ) -> Result<Json<Acknowledged>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;
        let limits = *instrument.limits();

        let (start, stop) = match (args.start_hz, args.stop_hz, args.center_hz, args.span_hz) {
            (Some(a), Some(b), _, _) => (Some(a), Some(b)),
            (_, _, Some(c), Some(s)) => (Some(c - s / 2.0), Some(c + s / 2.0)),
            (a, b, _, _) => (a, b),
        };
        if let (Some(a), Some(b)) = (start, stop) {
            limits.check_span(a, b).map_err(|e| mcp_err(e.into()))?;
        }
        if let Some(rbw) = args.rbw_hz {
            limits.check_rbw(rbw).map_err(|e| mcp_err(e.into()))?;
        }

        instrument.set_mode(Mode::Sa).await.map_err(mcp_err)?;
        if let Some(a) = start {
            instrument
                .set(&format!("SA:FREQ:START {a}"))
                .await
                .map_err(mcp_err)?;
        }
        if let Some(b) = stop {
            instrument
                .set(&format!("SA:FREQ:STOP {b}"))
                .await
                .map_err(mcp_err)?;
        }
        if let Some(rbw) = args.rbw_hz {
            instrument
                .set(&format!("SA:ACQ:RBW {rbw}"))
                .await
                .map_err(mcp_err)?;
        }
        if let Some(window) = &args.window {
            instrument
                .set(&format!("SA:ACQ:WINDOW {window}"))
                .await
                .map_err(mcp_err)?;
        }
        if let Some(detector) = &args.detector {
            instrument
                .set(&format!("SA:ACQ:DETECTOR {detector}"))
                .await
                .map_err(mcp_err)?;
        }
        if let Some(averaging) = args.averaging {
            instrument
                .set(&format!("SA:ACQ:AVG {averaging}"))
                .await
                .map_err(mcp_err)?;
        }

        Ok(Json(Acknowledged {
            ok: true,
            detail: "Spectrum analyser configured.".into(),
        }))
    }

    #[tool(description = "Run one spectrum-analyser sweep and wait for it to finish.")]
    pub async fn sa_sweep(
        &self,
        Parameters(args): Parameters<SweepArgs>,
    ) -> Result<Json<Acknowledged>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;

        let timeout = args
            .timeout_s
            .map(Duration::from_secs)
            .unwrap_or(self.config.sweep_timeout);
        instrument.set_mode(Mode::Sa).await.map_err(mcp_err)?;
        instrument.run_sa_sweep(timeout).await.map_err(mcp_err)?;

        Ok(Json(Acknowledged {
            ok: true,
            detail: "Spectrum sweep complete.".into(),
        }))
    }

    #[tool(
        description = "Read the spectrum as a peak table plus an estimated noise floor, rather \
                       than every point. Peaks are grouped so one carrier yields one entry."
    )]
    pub async fn sa_read(
        &self,
        Parameters(args): Parameters<SaReadArgs>,
    ) -> Result<Json<SaReadResult>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;

        let trace = resolve_trace(instrument, args.trace, TraceKind::Sa).await?;
        let points = instrument.read_sa_trace(&trace).await.map_err(mcp_err)?;
        let flags = instrument.status().await.map_err(mcp_err)?;

        // Estimate the floor as the median level, which a few strong carriers
        // cannot drag upwards the way a mean would.
        let mut levels: Vec<f64> = points.iter().map(|p| p.y).collect();
        levels.sort_by(f64::total_cmp);
        let noise_floor_dbm = levels.get(levels.len() / 2).copied().unwrap_or(f64::NAN);

        let floor = args.floor_dbm.unwrap_or(noise_floor_dbm + 10.0);
        let span = points
            .last()
            .zip(points.first())
            .map(|(l, f)| l.x - f.x)
            .unwrap_or(0.0);

        Ok(Json(SaReadResult {
            trace,
            points: points.len(),
            // Group peaks no closer than 1% of the span.
            peaks: analysis::find_peaks(&points, floor, span / 100.0, args.max_peaks.unwrap_or(10)),
            noise_floor_dbm,
            warnings: flags.warning().into_iter().collect(),
            flags,
        }))
    }

    #[tool(
        description = "Drive a continuous carrier out of a port. This radiates RF: make sure \
                       the port is terminated or connected to the intended load, not an \
                       antenna. Requires the emission tier."
    )]
    pub async fn gen_configure(
        &self,
        Parameters(args): Parameters<GenArgs>,
    ) -> Result<Json<Acknowledged>, ErrorData> {
        self.policy
            .require(Tier::Emission, "gen_configure")
            .map_err(|e| mcp_err(e.into()))?;

        if args.port != 1 && args.port != 2 {
            return Err(ErrorData::invalid_params(
                format!("port must be 1 or 2, got {}", args.port),
                None,
            ));
        }

        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;
        let limits = *instrument.limits();

        limits
            .check_frequency("generator frequency", args.freq_hz)
            .map_err(|e| mcp_err(e.into()))?;
        self.policy
            .check_stimulus(args.power_dbm, &limits)
            .map_err(|e| mcp_err(e.into()))?;

        instrument.set_mode(Mode::Gen).await.map_err(mcp_err)?;
        // Set frequency and level before enabling the port, so the carrier is
        // never briefly emitted at whatever the previous settings were.
        instrument
            .set(&format!("GEN:FREQUENCY {}", args.freq_hz))
            .await
            .map_err(mcp_err)?;
        instrument
            .set(&format!("GEN:LVL {}", args.power_dbm))
            .await
            .map_err(mcp_err)?;
        instrument
            .set(&format!("GEN:PORT {}", args.port))
            .await
            .map_err(mcp_err)?;

        Ok(Json(Acknowledged {
            ok: true,
            detail: format!(
                "Emitting {} dBm at {} Hz from port {}. Call gen_off or librevna_rf_off to stop.",
                args.power_dbm, args.freq_hz, args.port
            ),
        }))
    }

    #[tool(
        description = "Stop the signal generator. Always available, whatever tiers are enabled."
    )]
    pub async fn gen_off(&self) -> Result<Json<Acknowledged>, ErrorData> {
        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;
        instrument.set("GEN:PORT 0").await.map_err(mcp_err)?;
        Ok(Json(Acknowledged {
            ok: true,
            detail: "Generator output disabled.".into(),
        }))
    }

    #[tool(
        description = "Send a raw SCPI command. An escape hatch for anything the typed tools \
                       do not cover, including the MANUAL: hardware subsystem. No validation \
                       is applied. Requires the manual-hardware tier; firmware update is \
                       refused regardless."
    )]
    pub async fn scpi_raw(
        &self,
        Parameters(args): Parameters<RawArgs>,
    ) -> Result<Json<RawResult>, ErrorData> {
        self.policy
            .require(Tier::ManualHardware, "scpi_raw")
            .map_err(|e| mcp_err(e.into()))?;

        let upper = args.command.to_ascii_uppercase();
        if let Some(denied) = PERMANENTLY_DENIED.iter().find(|d| {
            upper
                .split_whitespace()
                .next()
                .is_some_and(|v| v.starts_with(*d))
        }) {
            return Err(mcp_err(
                crate::error::SafetyError::PermanentlyDenied((*denied).to_string()).into(),
            ));
        }

        let mut guard = self.instrument.lock().await;
        let instrument = guard
            .as_mut()
            .ok_or_else(|| mcp_err(VnaError::NotConnected))?;
        let response = instrument
            .raw_command(&args.command)
            .await
            .map_err(mcp_err)?;

        Ok(Json(RawResult {
            command: args.command,
            response,
        }))
    }
}

enum TraceKind {
    Vna,
    Sa,
}

/// Resolve an optional trace name to a concrete one, defaulting to the first.
async fn resolve_trace(
    instrument: &mut Instrument,
    requested: Option<String>,
    kind: TraceKind,
) -> Result<String, ErrorData> {
    if let Some(trace) = requested {
        return Ok(trace);
    }
    let available = match kind {
        TraceKind::Vna => instrument.vna_traces().await,
        TraceKind::Sa => instrument.sa_traces().await,
    }
    .map_err(mcp_err)?;

    available.into_iter().next().ok_or_else(|| {
        ErrorData::invalid_params(
            "no traces are defined; create one first with vna_trace_manage".to_string(),
            None,
        )
    })
}

fn require_value(value: &Option<String>, message: &str) -> Result<String, ErrorData> {
    value
        .clone()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| ErrorData::invalid_params(message.to_string(), None))
}

#[tool_handler]
impl ServerHandler for LibreVnaServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "Controls a LibreVNA vector network analyser (100 kHz - 6 GHz, 2 ports) \
                 through LibreVNA-GUI.\n\n\
                 Call librevna_connect first; it reports the device's own limits and which \
                 capability tiers are enabled.\n\n\
                 Measurement results carry two things worth reading before trusting the \
                 numbers: `flags`, the instrument's live error state, and `calibration`, \
                 which says whether the active calibration still covers the current sweep. \
                 An interpolated or invalid calibration still produces smooth, plausible \
                 traces -- they are just wrong.\n\n\
                 Read tools return summaries and a min/max envelope rather than every point; \
                 use vna_export_touchstone for full-resolution data.\n\n\
                 The RF ports have no input protection and are damaged above +10 dBm. \
                 librevna_rf_off and gen_off stop all emission and are always available."
                .into(),
        );
        info
    }
}
