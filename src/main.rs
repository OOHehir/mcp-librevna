//! Entry point: parse the capability flags, obtain a SCPI connection, and serve
//! MCP over stdio.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::stdio;

use mcp_librevna::gui;
use mcp_librevna::mock::MockServer;
use mcp_librevna::safety::{DEFAULT_MAX_STIMULUS_DBM, Policy};
use mcp_librevna::scpi::client::DEFAULT_SCPI_PORT;
use mcp_librevna::server::{LibreVnaServer, ServerConfig};

#[derive(Parser, Debug)]
#[command(
    name = "mcp-librevna",
    about = "MCP server for the LibreVNA vector network analyser",
    version
)]
struct Cli {
    /// Host serving SCPI (the machine running LibreVNA-GUI).
    #[arg(long, default_value = "127.0.0.1", env = "LIBREVNA_HOST")]
    host: String,

    /// SCPI port LibreVNA-GUI is listening on.
    #[arg(long, default_value_t = DEFAULT_SCPI_PORT, env = "LIBREVNA_PORT")]
    port: u16,

    /// Path to the LibreVNA-GUI binary, for --spawn.
    #[arg(long, env = "LIBREVNA_GUI_PATH")]
    gui_path: Option<PathBuf>,

    /// Start a headless LibreVNA-GUI if none is already listening.
    #[arg(long, env = "LIBREVNA_SPAWN")]
    spawn: bool,

    /// Serve against a built-in simulated instrument. No hardware or GUI needed.
    #[arg(long)]
    mock: bool,

    /// Directory calibration and Touchstone files are confined to.
    #[arg(long, env = "LIBREVNA_WORKDIR")]
    workdir: Option<PathBuf>,

    /// Ceiling on stimulus and generator power, in dBm.
    #[arg(long, default_value_t = DEFAULT_MAX_STIMULUS_DBM, env = "LIBREVNA_MAX_STIMULUS_DBM")]
    max_stimulus_dbm: f64,

    /// Allow RF emission: the signal generator and the tracking generator.
    #[arg(long, env = "LIBREVNA_ALLOW_EMISSION")]
    allow_emission: bool,

    /// Allow operations that discard state or write files.
    #[arg(long, env = "LIBREVNA_ALLOW_DESTRUCTIVE")]
    allow_destructive: bool,

    /// Allow raw SCPI and the MANUAL: hardware subsystem.
    #[arg(long, env = "LIBREVNA_ALLOW_MANUAL_HARDWARE")]
    allow_manual_hardware: bool,

    /// Timeout for an ordinary SCPI exchange, in seconds.
    #[arg(long, default_value_t = 10)]
    timeout_s: u64,

    /// Timeout for a sweep to complete, in seconds.
    #[arg(long, default_value_t = 120)]
    sweep_timeout_s: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // stdout carries the MCP protocol, so diagnostics must go to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mcp_librevna=info".into()),
        )
        .init();

    let workdir = match cli.workdir {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    std::fs::create_dir_all(&workdir)?;

    let policy = Policy::new(&workdir)?
        .with_emission(cli.allow_emission)
        .with_destructive(cli.allow_destructive)
        .with_manual_hardware(cli.allow_manual_hardware)
        .with_max_stimulus_dbm(cli.max_stimulus_dbm);

    // Held for the lifetime of the process: dropping either would tear down the
    // instrument this server is serving.
    let _mock_guard;
    let _gui_guard;

    let addr = if cli.mock {
        let mock = MockServer::spawn().await?;
        let addr = mock.addr();
        tracing::warn!(
            addr = %addr,
            "serving a SIMULATED LibreVNA -- readings are synthetic, not measurements"
        );
        _mock_guard = mock;
        addr
    } else {
        let addr = format!("{}:{}", cli.host, cli.port);
        _gui_guard = gui::ensure_available(&addr, cli.gui_path.as_deref(), cli.spawn).await?;
        addr
    };

    let config = ServerConfig {
        addr,
        timeout: Duration::from_secs(cli.timeout_s),
        sweep_timeout: Duration::from_secs(cli.sweep_timeout_s),
    };

    tracing::info!(
        addr = %config.addr,
        workdir = %workdir.display(),
        max_stimulus_dbm = cli.max_stimulus_dbm,
        emission = cli.allow_emission,
        destructive = cli.allow_destructive,
        manual_hardware = cli.allow_manual_hardware,
        "mcp-librevna ready"
    );

    let service = LibreVnaServer::new(config, policy).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
