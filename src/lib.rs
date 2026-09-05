//! An MCP server for the LibreVNA 100 kHz - 6 GHz 2-port vector network analyser.
//!
//! The server speaks SCPI over TCP to `LibreVNA-GUI --no-gui`, which owns the
//! USB link and implements calibration and de-embedding. See the README for the
//! capability tiers that gate the tool surface.

pub mod analysis;
pub mod autocal;
pub mod calibration;
pub mod device;
pub mod error;
pub mod gui;
pub mod instrument;
pub mod librecal;
pub mod mock;
pub mod safety;
pub mod scpi;
pub mod server;

pub use error::{Result, SafetyError, Tier, VnaError};
