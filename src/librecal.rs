//! Control of the LibreCAL electronic calibration module.
//!
//! The module is optional: most users will not have one, so nothing here is on
//! the path of an ordinary measurement. [`discover`] reports whether one is
//! attached, and every tool that needs one says so plainly when it is missing.
//!
//! Unlike the VNA, this device is spoken to directly over its USB CDC-ACM
//! interface rather than through LibreVNA-GUI. That is not a preference. The
//! GUI's own LibreCAL support lives entirely in `LibreCALDialog`, a Qt dialog;
//! its `CALibration` SCPI node exposes only ACTivate, ACTIVE, NUMber, RESET,
//! ADD, TYPE, PORT, STANDARD, MEASure, SAVE and LOAD. Under `--no-gui` there is
//! no way to ask the GUI to drive the module, so the server drives it itself.
//!
//! Three properties of the firmware, confirmed byte for byte against a v0.3.0
//! unit, shape this module:
//!
//! - **Every exchange answers with exactly one line.** A command that succeeds
//!   returns an empty line, `\r\n`; anything rejected returns `ERROR`, and so
//!   does a command the firmware does not recognise. This is the opposite of
//!   the VNA behind LibreVNA-GUI, where an unrecognised command is answered
//!   with silence and so costs a full timeout and desynchronises the session.
//!   Here a misspelt name comes back promptly and unambiguously, and silence
//!   means the module is gone rather than that the request was refused.
//! - **Reads must end on structure, never on a timer.** Waiting a fixed period
//!   and taking whatever arrived will collect a slow reply as the *next*
//!   exchange's answer, shifting every later one onto the wrong question.
//!   Reads here end on a complete line or the `END` sentinel, and a
//!   partly-read response poisons the session rather than being papered over.
//! - **Bulk responses are framed by literal `START` and `END` lines**, unlike
//!   the VNA's blank-line-terminated Touchstone. Two bulk framings now exist in
//!   this server and they are not interchangeable.
//!
//! The command set is discoverable from the device itself with `*LST?`, which
//! is more trustworthy than the published PDF.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

use crate::error::{Result, VnaError};

/// USB vendor ID shared by the LibreVNA project's devices.
pub const USB_VID: u16 = 0x1209;

/// USB product ID of the LibreCAL module. The VNA itself is `0x4121`.
pub const USB_PID: u16 = 0x4122;

/// The module presents a CDC-ACM interface, which ignores the line rate. A
/// value is required by the API regardless.
const BAUD: u32 = 115_200;

/// What a port can be terminated into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standard {
    Open,
    Short,
    Load,
    /// Not terminated. Electrically indistinguishable from an unconnected port,
    /// which is why it can never serve as evidence of a working connection.
    None,
    /// Connected through to another port. The firmware sets both ends.
    Through(u8),
}

impl Standard {
    /// The wire form, as the argument to `PORT <n>`.
    pub fn to_scpi(self) -> String {
        match self {
            Standard::Open => "OPEN".into(),
            Standard::Short => "SHORT".into(),
            Standard::Load => "LOAD".into(),
            Standard::None => "NONE".into(),
            Standard::Through(other) => format!("THROUGH {other}"),
        }
    }

    /// Parse a `PORT? <n>` response.
    pub fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.split_whitespace();
        match parts.next()?.to_ascii_uppercase().as_str() {
            "OPEN" => Some(Standard::Open),
            "SHORT" => Some(Standard::Short),
            "LOAD" => Some(Standard::Load),
            "NONE" => Some(Standard::None),
            "THROUGH" => parts.next()?.parse().ok().map(Standard::Through),
            _ => None,
        }
    }
}

impl std::fmt::Display for Standard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_scpi())
    }
}

/// A LibreCAL found on the USB bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModule {
    /// The character device to open, e.g. `/dev/ttyACM0`.
    pub path: PathBuf,
    /// USB serial number, which is also what `*IDN?` reports.
    pub usb_serial: String,
}

/// What the module reports about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalIdentity {
    pub serial: String,
    pub firmware: String,
}

/// Every LibreCAL currently attached, in stable order.
///
/// Enumerated from sysfs rather than by scanning `/dev/ttyACM*` blindly: the
/// number moves between plug-ins, and other CDC-ACM devices share the name.
/// Doing it here also keeps `libudev` out of the build, so the crate still
/// compiles without a C toolchain.
pub fn discover() -> Vec<DiscoveredModule> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/tty") else {
        return found;
    };

    let mut names: Vec<_> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("ttyACM"))
        .collect();
    names.sort();

    for name in names {
        // For a CDC-ACM tty, `device` is the USB *interface*; its parent is the
        // USB device that carries the vendor and product IDs.
        let usb = Path::new("/sys/class/tty").join(&name).join("device/..");
        let hex = |file: &str| -> Option<u16> {
            let raw = std::fs::read_to_string(usb.join(file)).ok()?;
            u16::from_str_radix(raw.trim(), 16).ok()
        };
        if hex("idVendor") != Some(USB_VID) || hex("idProduct") != Some(USB_PID) {
            continue;
        }
        found.push(DiscoveredModule {
            path: Path::new("/dev").join(&name),
            usb_serial: std::fs::read_to_string(usb.join("serial"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default(),
        });
    }
    found
}

/// An open session with a LibreCAL module.
pub struct LibreCal {
    reader: BufReader<SerialStream>,
    timeout: Duration,
    path: String,
    /// Set once a reply was left unread, which shifts every later answer onto
    /// the wrong question. Unrecoverable without reopening the port.
    desynchronised: bool,
}

impl LibreCal {
    /// Open the module at `path` and confirm it really is a LibreCAL.
    ///
    /// The identity check is not ceremony: `/dev/ttyACM*` numbering is not
    /// stable, and issuing `PORT 1 LOAD` to some other CDC-ACM device is worth
    /// avoiding.
    pub async fn open(path: &Path, timeout: Duration) -> Result<(Self, CalIdentity)> {
        let display = path.display().to_string();
        let port = tokio_serial::new(&display, BAUD)
            .timeout(timeout)
            .open_native_async()
            .map_err(|e| VnaError::LibreCalUnavailable {
                reason: format!("could not open {display}: {e}"),
            })?;

        let mut cal = Self {
            reader: BufReader::new(port),
            timeout,
            path: display.clone(),
            desynchronised: false,
        };

        let raw = cal.query("*IDN?").await?;
        let identity = parse_identity(&raw).ok_or_else(|| VnaError::LibreCalUnavailable {
            reason: format!("{display} answered *IDN? with {raw:?}, which is not a LibreCAL"),
        })?;
        Ok((cal, identity))
    }

    /// Open the first module found, if there is one.
    pub async fn open_first(timeout: Duration) -> Result<(Self, CalIdentity)> {
        let found = discover();
        let module = found.first().ok_or(VnaError::NoLibreCal)?;
        Self::open(&module.path, timeout).await
    }

    /// The device path this session is bound to.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// How many ports this module has. Four on current hardware.
    pub async fn port_count(&mut self) -> Result<u8> {
        self.query_parsed("PORTS?").await
    }

    /// Internal temperature in degrees Celsius.
    pub async fn temperature_c(&mut self) -> Result<f64> {
        self.query_parsed("TEMPerature?").await
    }

    /// Whether the oven has settled.
    ///
    /// The factory coefficients are only valid at the regulated temperature, so
    /// a calibration measured before this reads true is quietly wrong rather
    /// than obviously broken. Callers treat it as a precondition, not advice.
    pub async fn temperature_stable(&mut self) -> Result<bool> {
        let raw = self.query("TEMPerature:STABLE?").await?;
        match raw.trim().to_ascii_uppercase().as_str() {
            "TRUE" => Ok(true),
            "FALSE" => Ok(false),
            _ => Err(VnaError::Parse {
                command: "TEMPerature:STABLE?".into(),
                reason: "expected TRUE or FALSE".into(),
                raw,
            }),
        }
    }

    /// Heater power draw in watts.
    pub async fn heater_power_w(&mut self) -> Result<f64> {
        self.query_parsed("HEATer:POWer?").await
    }

    /// What `port` is currently terminated into.
    pub async fn standard(&mut self, port: u8) -> Result<Standard> {
        let command = format!("PORT? {port}");
        let raw = self.query(&command).await?;
        Standard::parse(&raw).ok_or(VnaError::Parse {
            command,
            reason: "expected OPEN, SHORT, LOAD, NONE or THROUGH <port>".into(),
            raw,
        })
    }

    /// Terminate `port` into `standard`, and confirm it took.
    ///
    /// The acknowledgement to the set proves only that the firmware parsed and
    /// accepted the command. The readback proves the port is actually in the
    /// state we asked for, which is the thing a calibration then depends on: a
    /// standard that never engaged leaves numbers that look entirely normal.
    pub async fn set_standard(&mut self, port: u8, standard: Standard) -> Result<()> {
        self.command(&format!("PORT {port} {}", standard.to_scpi()))
            .await?;

        let actual = self.standard(port).await?;
        if actual == standard {
            return Ok(());
        }
        // A through sets both ends, so the far port reports the reciprocal.
        if let (Standard::Through(a), Standard::Through(b)) = (standard, actual)
            && a == port
            && b == port
        {
            return Ok(());
        }
        Err(VnaError::LibreCalRefused(format!(
            "port {port} was set to {standard} but reads back as {actual}"
        )))
    }

    /// Set every port to [`Standard::None`].
    ///
    /// Used to reach a known state before a check and to leave one after, so a
    /// later measurement cannot silently inherit a termination.
    pub async fn clear_all(&mut self, ports: u8) -> Result<()> {
        for port in 1..=ports {
            self.set_standard(port, Standard::None).await?;
        }
        Ok(())
    }

    /// Names of the coefficient sets stored on the module, e.g. `FACTORY`.
    pub async fn coefficient_sets(&mut self) -> Result<Vec<String>> {
        let raw = self.query("COEFFicient:LIST?").await?;
        Ok(raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// One standard's factory coefficients, as Touchstone text.
    ///
    /// `name` is `P<n>_OPEN`, `P<n>_SHORT`, `P<n>_LOAD` or `P<lo><hi>_THROUGH`,
    /// where a through's ports must ascend: `P12_THROUGH` exists, `P21_THROUGH`
    /// is rejected.
    pub async fn coefficients(&mut self, set: &str, name: &str) -> Result<String> {
        self.query_block(&format!("COEFFicient:GET? {set} {name}"))
            .await
    }

    // ------------------------------------------------------------ transport

    /// Send a query and return its single-line response.
    async fn query(&mut self, command: &str) -> Result<String> {
        self.send(command).await?;
        let line = self.read_line(command, self.timeout).await?;
        if line.trim() == "ERROR" {
            return Err(VnaError::LibreCalRefused(command.to_string()));
        }
        Ok(line)
    }

    async fn query_parsed<T: std::str::FromStr>(&mut self, command: &str) -> Result<T> {
        let raw = self.query(command).await?;
        raw.trim().parse().map_err(|_| VnaError::Parse {
            command: command.to_string(),
            reason: format!("expected a {}", std::any::type_name::<T>()),
            raw,
        })
    }

    /// Send a command and read its one-line acknowledgement.
    ///
    /// An empty line means accepted and `ERROR` means rejected, so unlike the
    /// VNA there is no waiting out a timeout to discover which. A timeout here
    /// is a real fault -- the module has stopped answering -- rather than the
    /// firmware's way of saying no.
    async fn command(&mut self, command: &str) -> Result<()> {
        self.send(command).await?;
        let line = self.read_line(command, self.timeout).await?;
        match line.trim() {
            "" => Ok(()),
            "ERROR" => Err(VnaError::LibreCalRefused(command.to_string())),
            _ => Err(VnaError::Parse {
                command: command.to_string(),
                reason: "expected an empty line for success or ERROR for refusal".into(),
                raw: line,
            }),
        }
    }

    /// Read a `START` / `END` framed bulk response.
    ///
    /// The sentinels are what make this safe to read to completion. Ending on a
    /// pause instead would truncate a slow transfer and leave the remainder to
    /// be misread as the next reply.
    async fn query_block(&mut self, command: &str) -> Result<String> {
        self.send(command).await?;

        let first = self.read_line(command, self.timeout).await?;
        if first.trim() == "ERROR" {
            return Err(VnaError::LibreCalRefused(command.to_string()));
        }
        if first.trim() != "START" {
            return Err(VnaError::Parse {
                command: command.to_string(),
                reason: "expected a block opening with START".into(),
                raw: first,
            });
        }

        let mut body = String::new();
        loop {
            let line = self
                .read_line(command, self.timeout)
                .await
                .inspect_err(|_| {
                    // The rest of the block is still queued with no way to know how
                    // much; the session cannot be trusted again.
                    self.desynchronised = true;
                })?;
            if line.trim() == "END" {
                return Ok(body);
            }
            body.push_str(&line);
            body.push('\n');
        }
    }

    async fn read_line(&mut self, command: &str, timeout: Duration) -> Result<String> {
        let mut line = String::new();
        let read = tokio::time::timeout(timeout, self.reader.read_line(&mut line))
            .await
            .map_err(|_| VnaError::Timeout(timeout, command.to_string()))?;
        match read? {
            0 => Err(VnaError::LibreCalUnavailable {
                reason: format!("{} closed while awaiting a response", self.path),
            }),
            _ => Ok(line.trim_end_matches(['\r', '\n']).to_string()),
        }
    }

    async fn send(&mut self, command: &str) -> Result<()> {
        if self.desynchronised {
            return Err(VnaError::LibreCalUnavailable {
                reason: format!(
                    "the link to {} is out of step after a partly-read response; \
                     reconnect before issuing further commands",
                    self.path
                ),
            });
        }
        tracing::trace!(command, "librecal tx");
        let port = self.reader.get_mut();
        port.write_all(command.as_bytes()).await?;
        port.write_all(b"\n").await?;
        port.flush().await?;
        Ok(())
    }
}

// ------------------------------------------------------- connection check

/// Worst reflection, in dB, still accepted as the module's LOAD.
///
/// A LOAD reads about -23 dB uncalibrated across 100 MHz - 6 GHz on a healthy
/// setup and an unterminated port about -6 dB, so this sits in open ground
/// between them with room for a lossy cable or a narrower band.
pub const MAX_LOAD_DB: f64 = -15.0;

/// Least change between LOAD and OPEN, in dB, accepted as the module switching.
///
/// Measured at about 16 dB on a working setup. Any static termination -- a
/// bare port or a 50 ohm cap screwed on -- reads the same under both commands
/// and so changes by well under 1 dB.
pub const MIN_CHANGE_DB: f64 = 6.0;

/// What a port's LOAD-then-OPEN sequence showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortVerdict {
    /// The module is attached to this port and answering commands.
    Controllable,
    /// Reflection stayed high under both commands: nothing is attached.
    NotConnected,
    /// Reflection stayed low under both commands. Something absorbing is
    /// attached, but it is not switching -- a plain 50 ohm termination rather
    /// than the module.
    StaticTermination,
    /// The two readings fit no expected pattern.
    Inconclusive,
}

impl PortVerdict {
    pub fn is_ok(self) -> bool {
        matches!(self, PortVerdict::Controllable)
    }

    /// What this verdict means, in terms a caller can act on.
    pub fn explain(self) -> &'static str {
        match self {
            PortVerdict::Controllable => {
                "the LibreCAL is connected to this port and switching under command"
            }
            PortVerdict::NotConnected => {
                "reflection did not drop when the LibreCAL was set to LOAD, so nothing \
                 is connected to this VNA port -- check the RF cable"
            }
            PortVerdict::StaticTermination => {
                "reflection was low but did not change between LOAD and OPEN, so a fixed \
                 termination is attached rather than the LibreCAL -- check that the cable \
                 goes to the module and not to a 50 ohm load"
            }
            PortVerdict::Inconclusive => {
                "the LOAD and OPEN readings fit no expected pattern; check the cabling \
                 and that the sweep covers a sensible band"
            }
        }
    }
}

/// Judge one port from its mean reflection under LOAD and under OPEN.
///
/// Both figures are mean |reflection| in dB over the swept band, uncalibrated.
///
/// Testing the *change* rather than either level alone is what distinguishes a
/// module under our control from a termination that merely looks right. A
/// LOAD-only check passes a 50 ohm cap screwed onto the port, and an OPEN-only
/// check passes a port with no cable at all, since a disconnected port is an
/// open. Only the pair separates them.
pub fn assess_port(load_db: f64, open_db: f64) -> PortVerdict {
    let absorbs = load_db < MAX_LOAD_DB;
    let switched = open_db - load_db > MIN_CHANGE_DB;
    match (absorbs, switched) {
        (true, true) => PortVerdict::Controllable,
        (true, false) => PortVerdict::StaticTermination,
        (false, false) => PortVerdict::NotConnected,
        // Reflection changed by a lot but the LOAD never absorbed: neither
        // state matches what the module produces.
        (false, true) => PortVerdict::Inconclusive,
    }
}

/// Parse `LibreCAL,LibreCAL,<serial>,<firmware>`.
fn parse_identity(raw: &str) -> Option<CalIdentity> {
    let fields: Vec<&str> = raw.trim().split(',').map(str::trim).collect();
    if fields.len() < 4 || !fields[0].eq_ignore_ascii_case("LibreCAL") {
        return None;
    }
    Some(CalIdentity {
        serial: fields[2].to_string(),
        firmware: fields[3].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_recognised() {
        let id = parse_identity("LibreCAL,LibreCAL,5mV8QRceRywA,0.3.0").unwrap();
        assert_eq!(id.serial, "5mV8QRceRywA");
        assert_eq!(id.firmware, "0.3.0");
    }

    #[test]
    fn the_vna_is_not_mistaken_for_a_module() {
        // Both devices are CDC-capable and share a vendor ID, so this is the
        // check that stops PORT commands reaching the wrong one.
        assert!(parse_identity("LibreVNA,LibreVNA-GUI,374D34543435,1.6.5").is_none());
    }

    #[test]
    fn standards_round_trip() {
        for s in [
            Standard::Open,
            Standard::Short,
            Standard::Load,
            Standard::None,
            Standard::Through(2),
        ] {
            assert_eq!(Standard::parse(&s.to_scpi()), Some(s));
        }
    }

    #[test]
    fn a_through_reports_its_far_end() {
        assert_eq!(Standard::parse("THROUGH 1"), Some(Standard::Through(1)));
    }

    #[test]
    fn a_working_setup_is_recognised() {
        // Measured on hardware, 100 MHz - 6 GHz, uncalibrated:
        // cal P1 -> VNA 1 and cal P2 -> VNA 2.
        assert_eq!(assess_port(-23.43, -7.28), PortVerdict::Controllable);
        assert_eq!(assess_port(-24.01, -7.29), PortVerdict::Controllable);
    }

    #[test]
    fn an_unconnected_port_is_named_as_such() {
        // Both readings sit at the unterminated baseline.
        assert_eq!(assess_port(-6.41, -6.42), PortVerdict::NotConnected);
    }

    #[test]
    fn a_fixed_load_does_not_pass_as_the_module() {
        // The case a LOAD-only check would wave through: absorbing, but the
        // reading does not move when the module is told to open.
        assert_eq!(assess_port(-23.4, -23.4), PortVerdict::StaticTermination);
    }

    #[test]
    fn thresholds_keep_their_margin() {
        // The decision sits far from both measured populations rather than on
        // top of either, so a lossy cable does not flip it.
        assert!(assess_port(-16.0, -9.0).is_ok());
        assert!(!assess_port(-14.9, -8.0).is_ok());
    }

    #[test]
    fn unknown_terminations_are_rejected_rather_than_guessed() {
        assert_eq!(Standard::parse("ERROR"), None);
        assert_eq!(Standard::parse(""), None);
    }
}
