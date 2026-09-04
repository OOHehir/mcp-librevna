//! The connected instrument: state, sweep control and trace reads.
//!
//! Sits between the raw SCPI transport and the MCP tool surface. Everything an
//! agent can do to the hardware passes through here, which is where the
//! recorded calibration context and the device limits are kept in step with
//! what has actually been sent.

use std::time::Duration;

use crate::calibration::{CalValidity, CalibrationState, SweepConfig};
use crate::device::{DeviceLimits, Mode, StatusFlags};
use crate::error::{Result, VnaError};
use crate::scpi::parse::{ComplexPoint, Identity, ScalarPoint};
use crate::scpi::{ScpiClient, parse};

/// How often to ask whether a sweep has finished.
const SWEEP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A live connection to a LibreVNA through LibreVNA-GUI.
pub struct Instrument {
    client: ScpiClient,
    identity: Identity,
    limits: DeviceLimits,
    calibration: CalibrationState,
}

impl Instrument {
    /// Connect to LibreVNA-GUI and through it to the hardware.
    ///
    /// `serial` selects a specific unit when more than one is attached.
    pub async fn connect(addr: &str, timeout: Duration, serial: Option<&str>) -> Result<Self> {
        let mut client = ScpiClient::connect(addr, timeout).await?;

        let raw = client.query("*IDN?").await?;
        let identity = parse::parse_identity(&raw)?;

        match serial {
            Some(s) => client.command(&format!("DEV:CONN {s}")).await?,
            None => client.command("DEV:CONN").await?,
        }

        // DEV:CONN is silent when no device is attached, so confirm it took.
        let connected = client.query("DEV:CONN?").await?;
        if connected.trim().eq_ignore_ascii_case("Not connected") || connected.trim().is_empty() {
            let available = client.query("DEV:LIST?").await?;
            let devices = parse::parse_list(&available);
            return Err(VnaError::GuiUnavailable {
                addr: addr.to_string(),
                reason: if devices.is_empty() {
                    "LibreVNA-GUI is running but no LibreVNA is attached over USB".into()
                } else {
                    format!(
                        "could not connect to the requested device; available: {}",
                        devices.join(", ")
                    )
                },
            });
        }

        let limits = DeviceLimits::query(&mut client).await?;

        let mut instrument = Self {
            client,
            identity,
            limits,
            calibration: CalibrationState::default(),
        };
        instrument.refresh_calibration().await?;
        Ok(instrument)
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn limits(&self) -> &DeviceLimits {
        &self.limits
    }

    pub fn calibration(&self) -> &CalibrationState {
        &self.calibration
    }

    /// The serial number of the connected unit.
    pub async fn connected_serial(&mut self) -> Result<String> {
        self.client.query("DEV:CONN?").await
    }

    /// Release the device, leaving LibreVNA-GUI running.
    pub async fn disconnect(&mut self) -> Result<()> {
        self.client.command("DEV:DISC").await
    }

    pub async fn mode(&mut self) -> Result<Mode> {
        let raw = self.client.query("DEV:MODE?").await?;
        Mode::parse(&raw).ok_or_else(|| VnaError::Parse {
            command: "DEV:MODE?".into(),
            reason: "expected VNA, SA or GEN".into(),
            raw,
        })
    }

    pub async fn set_mode(&mut self, mode: Mode) -> Result<()> {
        self.client
            .command(&format!("DEV:MODE {}", mode.as_scpi()))
            .await
    }

    pub async fn status(&mut self) -> Result<StatusFlags> {
        StatusFlags::query(&mut self.client).await
    }

    pub async fn temperatures(&mut self) -> Result<String> {
        self.client.query("DEV:INF:TEMPERATURES?").await
    }

    /// Read back the sweep the instrument is currently configured for.
    pub async fn current_sweep(&mut self) -> Result<SweepConfig> {
        let start = self.query_f64("VNA:FREQ:START?").await?;
        let stop = self.query_f64("VNA:FREQ:STOP?").await?;
        let points = self.query_usize("VNA:ACQ:POINTS?").await?;
        Ok(SweepConfig::new(start, stop, points))
    }

    /// Ask the instrument which calibration is active and record the sweep.
    ///
    /// Called after connecting and after any calibration change, so the
    /// validity verdict reflects the instrument rather than our assumptions.
    pub async fn refresh_calibration(&mut self) -> Result<()> {
        let active = self.client.query("VNA:CAL:ACTIVE?").await?;
        let active = active.trim();

        if active.is_empty() || active.eq_ignore_ascii_case("none") {
            self.calibration.cleared();
            return Ok(());
        }

        let sweep = self.current_sweep().await?;
        self.calibration.applied(active, sweep);
        Ok(())
    }

    /// How far the active calibration can be trusted for the current sweep.
    pub async fn calibration_validity(&mut self) -> Result<CalValidity> {
        let current = self.current_sweep().await?;
        Ok(self.calibration.validity(&current))
    }

    /// Run one sweep and wait for it to complete.
    ///
    /// Averaging is respected: the wait is for `VNA:ACQ:FINISHED?`, which only
    /// goes true once the configured number of sweeps has been accumulated.
    pub async fn run_sweep(&mut self, timeout: Duration) -> Result<()> {
        self.client.command("VNA:ACQ:SINGLE TRUE").await?;
        self.client.command("VNA:ACQ:RUN").await?;
        self.wait_until_finished("VNA:ACQ:FINISHED?", timeout).await
    }

    /// Run one spectrum-analyser sweep and wait for it to complete.
    pub async fn run_sa_sweep(&mut self, timeout: Duration) -> Result<()> {
        self.client.command("SA:ACQ:SINGLE TRUE").await?;
        self.client.command("SA:ACQ:RUN").await?;
        self.wait_until_finished("SA:ACQ:FINISHED?", timeout).await
    }

    async fn wait_until_finished(&mut self, query: &str, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let raw = self.client.query(query).await?;
            if parse::parse_bool(query, &raw)? {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(VnaError::Timeout(timeout, query.to_string()));
            }
            tokio::time::sleep(SWEEP_POLL_INTERVAL).await;
        }
    }

    /// Stop a running acquisition in either mode.
    pub async fn stop_acquisition(&mut self) -> Result<()> {
        self.client.command("VNA:ACQ:STOP").await?;
        self.client.command("SA:ACQ:STOP").await
    }

    /// List the traces defined in VNA mode.
    pub async fn vna_traces(&mut self) -> Result<Vec<String>> {
        let raw = self.client.query("VNA:TRAC:LIST?").await?;
        Ok(parse::parse_list(&raw))
    }

    /// List the traces defined in spectrum-analyser mode.
    pub async fn sa_traces(&mut self) -> Result<Vec<String>> {
        let raw = self.client.query("SA:TRAC:LIST?").await?;
        Ok(parse::parse_list(&raw))
    }

    /// Read a complex VNA trace.
    ///
    /// Trace data can be large, so this is given a wider deadline than an
    /// ordinary query.
    pub async fn read_vna_trace(&mut self, trace: &str) -> Result<Vec<ComplexPoint>> {
        let command = format!("VNA:TRAC:DATA? {trace}");
        let budget = self.client.timeout().max(Duration::from_secs(30));
        let raw = self.client.query_with_timeout(&command, budget).await?;
        parse::parse_complex_trace(&command, &raw)
    }

    /// Read a scalar spectrum-analyser trace.
    pub async fn read_sa_trace(&mut self, trace: &str) -> Result<Vec<ScalarPoint>> {
        let command = format!("SA:TRAC:DATA? {trace}");
        let budget = self.client.timeout().max(Duration::from_secs(30));
        let raw = self.client.query_with_timeout(&command, budget).await?;
        parse::parse_scalar_trace(&command, &raw)
    }

    /// Ask the instrument to render traces as Touchstone text.
    ///
    /// The response is multi-line and ends with a blank line, so it is read with
    /// [`ScpiClient::query_multiline`]. The trace count is checked first: the
    /// instrument refuses an incomplete matrix silently, costing a full timeout.
    pub async fn touchstone(&mut self, traces: &[String]) -> Result<String> {
        check_touchstone_set(traces)?;
        let command = format!("VNA:TRAC:TOUCHSTONE? {}", traces.join(" "));
        let budget = self.client.timeout().max(Duration::from_secs(60));
        self.client.query_multiline(&command, budget).await
    }

    /// Turn off anything radiating from the ports.
    ///
    /// Deliberately best-effort across every emitting subsystem: this is the
    /// stop button, so it should not abort partway because one subsystem was
    /// not in a state that accepted the command.
    pub async fn all_rf_off(&mut self) -> Result<()> {
        let mut first_error = None;
        for command in ["GEN:PORT 0", "SA:TRACKING:ENABLE FALSE", "VNA:ACQ:STOP"] {
            if let Err(e) = self.client.command(command).await {
                tracing::warn!("rf-off step {command:?} failed: {e}");
                first_error.get_or_insert(e);
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Send a command with no interpretation, for the raw escape hatch.
    ///
    /// Read with [`ScpiClient::query_raw`], because an arbitrary command may
    /// answer in one line, in several closed by a blank one, or in several with
    /// no terminator. Assuming one line leaves the rest queued and shifts every
    /// later exchange onto the wrong request.
    pub async fn raw_command(&mut self, command: &str) -> Result<Option<String>> {
        // A query is identified by its verb, since arguments may follow the `?`.
        let verb = command.split_whitespace().next().unwrap_or(command);
        if verb.ends_with('?') {
            let budget = self.client.timeout();
            Ok(Some(self.client.query_raw(command, budget).await?))
        } else {
            self.client.command(command).await?;
            Ok(None)
        }
    }

    /// Set a value and confirm the instrument accepted it.
    pub async fn set(&mut self, command: &str) -> Result<()> {
        self.client.command(command).await
    }

    pub async fn query_f64(&mut self, command: &str) -> Result<f64> {
        let raw = self.client.query(command).await?;
        parse::parse_f64(command, &raw)
    }

    pub async fn query_usize(&mut self, command: &str) -> Result<usize> {
        let raw = self.client.query(command).await?;
        parse::parse_usize(command, &raw)
    }

    pub async fn query_bool(&mut self, command: &str) -> Result<bool> {
        let raw = self.client.query(command).await?;
        parse::parse_bool(command, &raw)
    }

    pub async fn query_string(&mut self, command: &str) -> Result<String> {
        self.client.query(command).await
    }
}

/// Check that a trace set forms a complete N-port S-matrix.
///
/// Touchstone stores an N x N matrix, so the instrument accepts only N^2 traces:
/// one for a 1-port, four for a 2-port. It answers anything else -- notably the
/// natural-looking `S11 S21` -- with silence, hence refusing before sending.
///
/// Only the count is checked: traces can be renamed freely in LibreVNA, so
/// requiring `S11`-shaped names would refuse valid exports.
fn check_touchstone_set(traces: &[String]) -> Result<()> {
    let n = traces.len();
    let ports = (n as f64).sqrt().round() as usize;
    if n > 0 && ports * ports == n {
        return Ok(());
    }
    Err(VnaError::InvalidRequest(format!(
        "Touchstone needs a complete N-port matrix of traces, so the count must be a \
         square: 1 for a 1-port (.s1p) or 4 for a 2-port (.s2p), in the order \
         S11 S12 S21 S22. Got {n}: {traces:?}. The instrument answers an incomplete \
         set with silence rather than an error, so this is refused before sending."
    )))
}

#[cfg(test)]
mod tests {
    use super::check_touchstone_set;

    fn set(traces: &[&str]) -> Vec<String> {
        traces.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn complete_port_matrices_are_accepted() {
        assert!(check_touchstone_set(&set(&["S11"])).is_ok());
        assert!(check_touchstone_set(&set(&["S11", "S12", "S21", "S22"])).is_ok());
    }

    #[test]
    fn renamed_traces_are_accepted_on_count_alone() {
        // LibreVNA lets traces be renamed; refusing these would be a false alarm.
        assert!(check_touchstone_set(&set(&["input_match"])).is_ok());
        assert!(check_touchstone_set(&set(&["a", "b", "c", "d"])).is_ok());
    }

    #[test]
    fn partial_matrices_are_refused_rather_than_timing_out() {
        // The instrument answers both of these with silence.
        for traces in [set(&["S11", "S21"]), set(&["S11", "S12", "S21"])] {
            let error = check_touchstone_set(&traces)
                .expect_err("an incomplete matrix should be refused")
                .to_string();
            assert!(error.contains("square"), "{error}");
            assert!(error.contains("S11 S12 S21 S22"), "{error}");
        }
    }

    #[test]
    fn an_empty_set_is_refused() {
        assert!(check_touchstone_set(&[]).is_err());
    }
}
