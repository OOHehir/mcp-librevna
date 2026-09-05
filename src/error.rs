//! Error types for the LibreVNA MCP server.

use std::fmt;

/// Errors arising from talking to, or being asked to misuse, the instrument.
#[derive(Debug, thiserror::Error)]
pub enum VnaError {
    /// The TCP connection to LibreVNA-GUI failed or dropped.
    #[error("SCPI transport error: {0}")]
    Transport(#[from] std::io::Error),

    /// A command did not produce a reply within the configured budget.
    #[error("SCPI timeout after {0:?} waiting for response to `{1}`")]
    Timeout(std::time::Duration, String),

    /// The instrument replied, but not with anything we could interpret.
    #[error("could not parse response to `{command}`: {reason} (got {raw:?})")]
    Parse {
        command: String,
        reason: String,
        raw: String,
    },

    /// The request was rejected before any SCPI was sent.
    #[error("{0}")]
    Safety(#[from] SafetyError),

    /// No device is connected, but the operation needs one.
    #[error("not connected to a LibreVNA (call librevna_connect first)")]
    NotConnected,

    /// A request the instrument would refuse, caught before it is sent.
    ///
    /// Distinct from [`SafetyError`], which is policy: this is a request the
    /// instrument will not honour. It answers such requests with silence, so
    /// anything caught here would otherwise surface as an unexplained timeout.
    #[error("{0}")]
    InvalidRequest(String),

    /// No LibreCAL module is attached.
    ///
    /// Not a fault: the module is optional, and every tool needing one says so
    /// rather than failing obscurely.
    #[error(
        "no LibreCAL module found on USB (looked for {vid:04x}:{pid:04x}). \
         Attach one, or calibrate with manual standards instead."
    , vid = crate::librecal::USB_VID, pid = crate::librecal::USB_PID)]
    NoLibreCal,

    /// The LibreCAL is attached but could not be talked to.
    #[error("LibreCAL is not usable: {reason}")]
    LibreCalUnavailable { reason: String },

    /// The module answered `ERROR`, or a set did not read back as commanded.
    #[error("LibreCAL rejected `{0}`")]
    LibreCalRefused(String),

    /// LibreVNA-GUI is not reachable.
    #[error("LibreVNA-GUI is not reachable on {addr}: {reason}")]
    GuiUnavailable { addr: String, reason: String },
}

/// A request refused by policy rather than by the instrument.
///
/// These are returned *before* anything is transmitted, so a refused request
/// never changes instrument state.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SafetyError {
    /// The operation needs a capability tier that was not enabled at startup.
    #[error(
        "`{operation}` requires the {tier} capability, which is not enabled. \
         Restart the server with {flag} to allow it."
    )]
    TierNotEnabled {
        operation: String,
        tier: Tier,
        flag: &'static str,
    },

    /// A parameter fell outside what this specific device reports it supports.
    #[error(
        "{parameter} = {value} {unit} is outside the range this device reports \
         ({min} to {max} {unit})"
    )]
    OutOfRange {
        parameter: String,
        value: f64,
        min: f64,
        max: f64,
        unit: &'static str,
    },

    /// Stimulus power above the configured ceiling.
    #[error(
        "stimulus power {requested} dBm exceeds the configured ceiling of {ceiling} dBm. \
         Raise it with --max-stimulus-dbm if the DUT can take it."
    )]
    PowerCeiling { requested: f64, ceiling: f64 },

    /// A file path escaped the working directory.
    #[error("path {path:?} resolves outside the working directory {workdir:?}")]
    PathEscape { path: String, workdir: String },

    /// A command that is never exposed, at any tier.
    #[error("`{0}` is not available through this server under any configuration")]
    PermanentlyDenied(String),
}

/// Capability tiers gating the tool surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Queries and status. Always available.
    ReadOnly,
    /// Sweep configuration, acquisition, trace management. On by default.
    Measure,
    /// Anything that radiates RF from a port.
    Emission,
    /// Anything that discards state or overwrites files.
    Destructive,
    /// Direct hardware register control.
    ManualHardware,
}

impl Tier {
    /// The CLI flag that enables this tier, if it is not on by default.
    pub fn flag(self) -> &'static str {
        match self {
            Tier::ReadOnly | Tier::Measure => "(enabled by default)",
            Tier::Emission => "--allow-emission",
            Tier::Destructive => "--allow-destructive",
            Tier::ManualHardware => "--allow-manual-hardware",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Tier::ReadOnly => "read-only",
            Tier::Measure => "measure",
            Tier::Emission => "emission",
            Tier::Destructive => "destructive",
            Tier::ManualHardware => "manual-hardware",
        };
        f.write_str(s)
    }
}

pub type Result<T> = std::result::Result<T, VnaError>;
