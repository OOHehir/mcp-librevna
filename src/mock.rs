//! A stateful in-process mock of the LibreVNA-GUI SCPI server.
//!
//! This exists because the instrument is not always present: it lets the whole
//! stack -- transport, parsing, safety policy and the MCP tool surface -- run
//! in CI and be exercised by hand without hardware.
//!
//! It is a behavioural stand-in, not a simulator. Settable keys round-trip
//! through their queries, sweeps synthesise a plausible resonator response, and
//! the reported limits are those a real LibreVNA v1 reports for itself.
//!
//! The response formats match a v1.6.5 GUI, including the two a forgiving mock
//! would paper over: an unrecognised query is answered with silence rather than
//! a blank line, and `VNA:TRAC:TOUCHSTONE?` answers over several lines closed by
//! a blank one, only for a complete N-port set of traces.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Faults the mock can be told to inject, so error paths are testable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Faults {
    /// Report the ADC as overloaded, as an over-driven input would.
    pub adc_overload: bool,
    /// Report the PLLs as unlocked.
    pub pll_unlocked: bool,
    /// Report the source as unable to reach the requested level.
    pub unlevel: bool,
    /// Never answer a sweep-completion query, to exercise timeout handling.
    pub stall_sweep: bool,
    /// Drop the connection on the next command.
    pub drop_connection: bool,
}

#[derive(Debug)]
struct MockState {
    settings: HashMap<String, String>,
    connected_serial: Option<String>,
    active_calibration: Option<String>,
    faults: Faults,
    /// Commands received, in order, so tests can assert nothing was sent.
    log: Vec<String>,
}

impl MockState {
    fn new() -> Self {
        let mut settings = HashMap::new();
        // Device-reported limits for a LibreVNA v1.
        for (k, v) in [
            ("DEV:INF:LIM:MINF", "0"),
            ("DEV:INF:LIM:MAXF", "6000000000"),
            ("DEV:INF:LIM:MINIFBW", "6"),
            ("DEV:INF:LIM:MAXIFBW", "50000"),
            ("DEV:INF:LIM:MAXPOINTS", "4501"),
            ("DEV:INF:LIM:MINPOW", "-40"),
            ("DEV:INF:LIM:MAXPOW", "0"),
            ("DEV:INF:LIM:MINRBW", "13"),
            ("DEV:INF:LIM:MAXRBW", "111500"),
            ("DEV:INF:FWREV", "1.4.0"),
            ("DEV:INF:HWREV", "1 Rev.B"),
            // Default sweep state.
            ("VNA:FREQ:START", "1000000"),
            ("VNA:FREQ:STOP", "6000000000"),
            ("VNA:ACQ:POINTS", "501"),
            ("VNA:ACQ:IFBW", "1000"),
            ("VNA:ACQ:AVG", "1"),
            ("VNA:ACQ:SINGLE", "FALSE"),
            ("VNA:STIM:LVL", "-10"),
            ("VNA:SWEEPTYPE", "LIN"),
            ("DEV:MODE", "VNA"),
            ("SA:FREQ:START", "1000000"),
            ("SA:FREQ:STOP", "6000000000"),
            ("SA:ACQ:RBW", "10000"),
            ("GEN:PORT", "0"),
            ("GEN:FREQUENCY", "1000000000"),
            ("GEN:LVL", "-20"),
        ] {
            settings.insert(k.to_string(), v.to_string());
        }
        Self {
            settings,
            connected_serial: None,
            active_calibration: None,
            faults: Faults::default(),
            log: Vec::new(),
        }
    }

    fn get(&self, key: &str) -> String {
        self.settings.get(key).cloned().unwrap_or_default()
    }

    fn num(&self, key: &str) -> f64 {
        self.get(key).parse().unwrap_or(0.0)
    }
}

/// A handle to a running mock server.
pub struct MockServer {
    addr: std::net::SocketAddr,
    state: Arc<Mutex<MockState>>,
}

impl MockServer {
    /// Start a mock on an ephemeral port and serve connections until dropped.
    pub async fn spawn() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(MockState::new()));

        let accept_state = Arc::clone(&state);
        tokio::spawn(async move {
            // Mirrors the real server: one client at a time, served in turn.
            while let Ok((stream, _)) = listener.accept().await {
                let conn_state = Arc::clone(&accept_state);
                if let Err(e) = serve_connection(stream, conn_state).await {
                    tracing::debug!("mock connection ended: {e}");
                }
            }
        });

        Ok(Self { addr, state })
    }

    /// The `host:port` this mock is listening on.
    pub fn addr(&self) -> String {
        self.addr.to_string()
    }

    /// Inject faults for the next exchanges.
    pub fn set_faults(&self, faults: Faults) {
        self.state.lock().unwrap().faults = faults;
    }

    /// Every command the mock has received, in order.
    pub fn command_log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    /// Read back a setting, to assert what the server actually applied.
    pub fn setting(&self, key: &str) -> String {
        self.state.lock().unwrap().get(key)
    }
}

async fn serve_connection(stream: TcpStream, state: Arc<Mutex<MockState>>) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        let command = line.trim().to_string();
        if command.is_empty() {
            continue;
        }

        let response = {
            let mut st = state.lock().unwrap();
            st.log.push(command.clone());
            if st.faults.drop_connection {
                return Ok(());
            }
            handle(&command, &mut st)
        };

        // Only queries produce output, and a request the instrument rejects
        // produces none either -- hence an Option rather than an error string.
        if let Some(reply) = response {
            let (text, blank_line) = match reply {
                Reply::Line(t) | Reply::Unframed(t) => (t, false),
                Reply::Block(t) => (t, true),
            };
            write_half.write_all(text.as_bytes()).await?;
            write_half.write_all(b"\n").await?;
            if blank_line {
                write_half.write_all(b"\n").await?;
            }
            write_half.flush().await?;
        }
    }
    Ok(())
}

/// How a response is framed on the wire.
///
/// The instrument uses all three, and the difference is the whole reason a
/// reader can desynchronise, so the mock reproduces each exactly.
enum Reply {
    /// One line: nearly every query.
    Line(String),
    /// Several lines closed by a blank one, as `VNA:TRAC:TOUCHSTONE?` sends.
    Block(String),
    /// Several lines with no terminator at all, as `*LST?` sends.
    Unframed(String),
}

/// Handle one command, returning a response only if it is a query.
fn handle(command: &str, st: &mut MockState) -> Option<Reply> {
    // A query is identified by its *verb* ending in `?`, not the whole line:
    // `VNA:TRAC:DATA? S11` carries an argument after the question mark.
    let (verb, argument) = match command.split_once(char::is_whitespace) {
        Some((v, a)) => (v, a.trim()),
        None => (command, ""),
    };
    let verb_upper = verb.to_ascii_uppercase();

    if let Some(key) = verb_upper.strip_suffix('?') {
        // Touchstone holds an N x N matrix: the instrument answers only for N^2
        // traces, and refuses an incomplete set with silence rather than error.
        if key == "VNA:TRAC:TOUCHSTONE" {
            let n = argument.split_whitespace().count();
            let ports = (n as f64).sqrt().round() as usize;
            if n == 0 || ports * ports != n {
                return None;
            }
        }
        // `*LST?` is multi-line and, unlike Touchstone, carries no terminator:
        // the reader has only quiescence to go on.
        if key == "*LST" {
            return Some(Reply::Unframed(command_list()));
        }
        let response = handle_query(key, argument, st);
        // An unrecognised query gets silence from the real server, not a blank
        // line. Answering "" here would let a misspelt command name pass CI.
        if response.is_empty() {
            tracing::debug!("mock has no value for query {key:?}; answering with silence");
            return None;
        }
        return Some(if key == "VNA:TRAC:TOUCHSTONE" {
            Reply::Block(response)
        } else {
            Reply::Line(response)
        });
    }

    // Sets: `KEY VALUE`, plus a few verbs that take no argument.
    let (key, value) = (verb_upper, argument.to_string());

    match key.as_str() {
        "*RST" => {
            let faults = st.faults;
            *st = MockState::new();
            st.faults = faults;
        }
        "*CLS" | "VNA:ACQ:RUN" | "VNA:ACQ:STOP" | "SA:ACQ:RUN" | "SA:ACQ:STOP" => {}
        "DEV:CONN" => {
            st.connected_serial = Some(if value.is_empty() {
                "MOCK0001".to_string()
            } else {
                value
            });
        }
        "DEV:DISC" => st.connected_serial = None,
        // An unknown type is ignored without complaint, exactly as the
        // instrument ignores it -- the caller sees success and no calibration.
        "VNA:CAL:ACTIVATE" => {
            if CAL_TYPES.iter().any(|t| t.eq_ignore_ascii_case(&value)) {
                st.active_calibration = Some(value);
            }
        }
        "VNA:CAL:RESET" => st.active_calibration = None,
        // Span and centre are derived, so keep them consistent with start/stop.
        "VNA:FREQ:SPAN" | "VNA:FREQ:CENTER" | "SA:FREQ:SPAN" | "SA:FREQ:CENTER" => {
            let prefix = if key.starts_with("VNA") { "VNA" } else { "SA" };
            apply_derived_frequency(prefix, &key, &value, st);
        }
        _ => {
            st.settings.insert(key, value);
        }
    }
    None
}

fn apply_derived_frequency(prefix: &str, key: &str, value: &str, st: &mut MockState) {
    let v: f64 = value.parse().unwrap_or(0.0);
    let start = st.num(&format!("{prefix}:FREQ:START"));
    let stop = st.num(&format!("{prefix}:FREQ:STOP"));
    let (new_start, new_stop) = if key.ends_with("SPAN") {
        let center = (start + stop) / 2.0;
        (center - v / 2.0, center + v / 2.0)
    } else {
        let span = stop - start;
        (v - span / 2.0, v + span / 2.0)
    };
    st.settings
        .insert(format!("{prefix}:FREQ:START"), new_start.to_string());
    st.settings
        .insert(format!("{prefix}:FREQ:STOP"), new_stop.to_string());
}

fn handle_query(key: &str, argument: &str, st: &mut MockState) -> String {
    let _ = argument; // Trace selection is not modelled; all traces read alike.

    match key {
        "*IDN" => "LibreVNA,LibreVNA-GUI,MOCK0001,1.4.0".into(),
        "*OPC" => "1".into(),

        "DEV:LIST" => "MOCK0001".into(),
        "DEV:CONN" => st
            .connected_serial
            .clone()
            .unwrap_or_else(|| "Not connected".into()),

        "DEV:STA:ADCOVERLOAD" => bool_str(st.faults.adc_overload),
        "DEV:STA:UNLEVEL" => bool_str(st.faults.unlevel),
        "DEV:STA:UNLOCKED" => bool_str(st.faults.pll_unlocked),
        "DEV:INF:TEMPERATURES" => "42/45/38".into(),

        // Averaging is reported complete unless deliberately stalled.
        "VNA:ACQ:FINISHED" | "SA:ACQ:FINISHED" => bool_str(!st.faults.stall_sweep),
        "VNA:ACQ:AVGLEVEL" | "SA:ACQ:AVGLEVEL" => st.get("VNA:ACQ:AVG"),
        "VNA:ACQ:RUN" | "SA:ACQ:RUN" => "TRUE".into(),
        "VNA:ACQ:LIMIT" | "SA:ACQ:LIMIT" => "PASS".into(),

        "VNA:TRAC:LIST" => "S11,S12,S21,S22".into(),
        "SA:TRAC:LIST" => "PORT1,PORT2".into(),

        "VNA:CAL:ACTIVE" => st
            .active_calibration
            .clone()
            .unwrap_or_else(|| "None".into()),
        "VNA:CAL:ACTIVATE" => CAL_TYPES.join(","),
        "VNA:CAL:BUSY" => "FALSE".into(),
        "VNA:CAL:NUMBER" => "0".into(),

        // Derived frequency queries.
        "VNA:FREQ:SPAN" => (st.num("VNA:FREQ:STOP") - st.num("VNA:FREQ:START")).to_string(),
        "VNA:FREQ:CENTER" => {
            ((st.num("VNA:FREQ:STOP") + st.num("VNA:FREQ:START")) / 2.0).to_string()
        }
        "SA:FREQ:SPAN" => (st.num("SA:FREQ:STOP") - st.num("SA:FREQ:START")).to_string(),
        "SA:FREQ:CENTER" => ((st.num("SA:FREQ:STOP") + st.num("SA:FREQ:START")) / 2.0).to_string(),

        "VNA:TRAC:DATA" => synth_complex_trace(st),
        "VNA:TRAC:TOUCHSTONE" => synth_touchstone(st),
        "SA:TRAC:DATA" => synth_scalar_trace(st),

        // Anything else is a plain settable key. An unknown one yields an
        // empty string, which the caller turns into silence.
        _ => st.get(key),
    }
}

/// The calibration types a v1.6.5 unit offers. Names are device-specific and
/// nothing shorter (`SOLT`, `TRL`) is accepted.
const CAL_TYPES: [&str; 8] = [
    "OSL_1",
    "OSL_2",
    "OSL_12",
    "SOLT_1",
    "SOLT_2",
    "SOLT_12",
    "ThroughNormalization_12",
    "TRL_12",
];

/// A `*LST?` reply: multi-line, no terminator. Trimmed to the commands this
/// crate sends; the instrument lists 270.
fn command_list() -> String {
    [
        "*CLS",
        "*ESE?",
        "*OPC?",
        "*OPC",
        "*LST?",
        "*IDN?",
        "*RST",
        "DEVice:CONNect?",
        "DEVice:LIST?",
        "DEVice:MODE?",
        "VNA:ACQuisition:RUN",
        "VNA:TRACe:LIST?",
        "VNA:TRACe:TOUCHSTONE?",
    ]
    .join("\n")
}

fn bool_str(b: bool) -> String {
    if b { "TRUE".into() } else { "FALSE".into() }
}

/// Synthesise an S11 notch resonance so analysis code has realistic input.
///
/// Models a single-pole resonator at 2.44 GHz with a loaded Q of 40: near the
/// resonance the reflection dips sharply, away from it the port looks like an
/// open. The shape matters more than the physics -- it gives tests a genuine
/// minimum, a real -3 dB bandwidth, and smooth data either side.
fn synth_complex_trace(st: &MockState) -> String {
    let start = st.num("VNA:FREQ:START");
    let stop = st.num("VNA:FREQ:STOP");
    let points: usize = st.get("VNA:ACQ:POINTS").parse().unwrap_or(501);
    let points = points.clamp(2, 4501);

    const F0: f64 = 2.44e9;
    const Q: f64 = 40.0;

    let step = (stop - start) / (points - 1) as f64;
    let mut out = String::new();
    for i in 0..points {
        let f = start + step * i as f64;
        // Detuning term of a single-pole resonator.
        let detune = Q * (f / F0 - F0 / f);
        let denom = 1.0 + detune * detune;
        // The residual is added inside the response rather than clamped on top,
        // so the dip keeps a smooth shape instead of flattening into a plateau.
        const RESIDUAL: f64 = 1.0e-3; // about -60 dB at the notch.
        let re = (detune * detune + RESIDUAL) / denom;
        let im = -detune / denom;
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("[{f:.1},{re:.9},{im:.9}]"));
    }
    out
}

/// Render the synthesised trace as Touchstone text.
///
/// Multi-line, with real newlines, as the instrument sends it. `serve_connection`
/// appends the newline closing the last row, and the blank line after it
/// terminates the response.
fn synth_touchstone(st: &MockState) -> String {
    let start = st.num("VNA:FREQ:START");
    let stop = st.num("VNA:FREQ:STOP");
    let points: usize = st.get("VNA:ACQ:POINTS").parse().unwrap_or(501);
    let points = points.clamp(2, 4501);

    const F0: f64 = 2.44e9;
    const Q: f64 = 40.0;
    const RESIDUAL: f64 = 1.0e-3;

    let step = (stop - start) / (points - 1) as f64;
    let mut out = String::from("! Synthesised by the mcp-librevna mock\n# Hz S RI R 50\n");
    for i in 0..points {
        let f = start + step * i as f64;
        let detune = Q * (f / F0 - F0 / f);
        let denom = 1.0 + detune * detune;
        let re = (detune * detune + RESIDUAL) / denom;
        let im = -detune / denom;
        // No trailing newline on the last row: serve_connection adds it, and the
        // blank terminator line follows from that.
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("{f:.1} {re:.9} {im:.9}"));
    }
    out
}

/// Synthesise a spectrum with a carrier peak above a noise floor.
///
/// The peak is a couple of bins wide rather than a fixed number of hertz. A
/// real analyser bins power into resolution cells, so a carrier stays visible
/// however wide the span is set; a fixed-width peak would fall between samples
/// at full span and vanish, which would make the mock misleading.
fn synth_scalar_trace(st: &MockState) -> String {
    let start = st.num("SA:FREQ:START");
    let stop = st.num("SA:FREQ:STOP");
    let points = 201usize;

    const CARRIER: f64 = 1.0e9;
    const NOISE_FLOOR_DBM: f64 = -95.0;
    const PEAK_DB: f64 = 65.0;

    let step = (stop - start) / (points - 1) as f64;
    // Never narrower than the bin spacing, never wider than the RBW would give.
    let width = step.max(st.num("SA:ACQ:RBW")).max(1.0);

    let mut out = String::new();
    for i in 0..points {
        let f = start + step * i as f64;
        let offset = (f - CARRIER) / width;
        let level = NOISE_FLOOR_DBM + PEAK_DB / (1.0 + offset * offset);
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("[{f:.1},{level:.3}]"));
    }
    out
}
