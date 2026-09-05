//! Locating, or starting, the LibreVNA-GUI that owns the USB link.
//!
//! The server can attach to a GUI the user already has open -- handy when they
//! want to watch traces on screen -- or start a headless one itself. Attaching
//! is tried first either way, because the SCPI server accepts a single client
//! and starting a second GUI would fight the first for the device.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::process::{Child, Command};

use crate::error::{Result, VnaError};

/// How long to wait for a freshly spawned GUI to open its SCPI port.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const STARTUP_POLL: Duration = Duration::from_millis(250);

/// How this server may obtain a GUI, when it is not talking to a mock.
#[derive(Debug, Clone, Default)]
pub struct GuiOptions {
    /// Path to the LibreVNA-GUI binary, needed to start one.
    pub path: Option<PathBuf>,
    /// Whether starting a headless GUI is permitted at all.
    pub spawn: bool,
}

/// A LibreVNA-GUI process started by this server, terminated when dropped.
pub struct ManagedGui {
    child: Child,
    path: PathBuf,
}

impl ManagedGui {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ManagedGui {
    fn drop(&mut self) {
        // We started it, so we clean it up; a stray headless GUI would hold the
        // USB device and block the next run.
        tracing::info!("stopping the LibreVNA-GUI we started");
        let _ = self.child.start_kill();
    }
}

/// How the server obtained its SCPI connection.
pub enum GuiSource {
    /// Something was already listening; we attached to it.
    Attached,
    /// We started a headless GUI ourselves.
    Spawned(ManagedGui),
}

/// Ensure a SCPI server is listening at `addr`, starting one if permitted.
pub async fn ensure_available(
    addr: &str,
    gui_path: Option<&Path>,
    allow_spawn: bool,
) -> Result<GuiSource> {
    if is_listening(addr).await {
        tracing::info!(addr, "attached to a LibreVNA-GUI that was already running");
        return Ok(GuiSource::Attached);
    }

    let Some(path) = gui_path.filter(|_| allow_spawn) else {
        return Err(VnaError::GuiUnavailable {
            addr: addr.to_string(),
            reason: if allow_spawn {
                "nothing is listening and no --gui-path was given. Start LibreVNA-GUI \
                 yourself, or pass --gui-path so this server can start one."
                    .into()
            } else {
                "nothing is listening. Start LibreVNA-GUI (it serves SCPI on port 19542 \
                 by default), or pass --spawn with --gui-path to have this server start \
                 a headless one."
                    .into()
            },
        });
    };

    let port = addr.rsplit(':').next().unwrap_or("19542");
    tracing::info!(?path, port, "starting a headless LibreVNA-GUI");

    let child = Command::new(path)
        .arg("--no-gui")
        .arg("-p")
        .arg(port)
        // LibreVNA-GUI is a Qt application; --no-gui still initialises Qt, which
        // wants a display unless told to render nowhere.
        .env("QT_QPA_PLATFORM", "offscreen")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| VnaError::GuiUnavailable {
            addr: addr.to_string(),
            reason: format!("could not start {}: {e}", path.display()),
        })?;

    let managed = ManagedGui {
        child,
        path: path.to_path_buf(),
    };

    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if is_listening(addr).await {
            tracing::info!(addr, "headless LibreVNA-GUI is serving SCPI");
            return Ok(GuiSource::Spawned(managed));
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }

    Err(VnaError::GuiUnavailable {
        addr: addr.to_string(),
        reason: format!(
            "started {} but it did not open the SCPI port within {STARTUP_TIMEOUT:?}",
            managed.path().display()
        ),
    })
}

/// Whether anything is accepting connections at `addr`.
pub async fn is_listening(addr: &str) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}
