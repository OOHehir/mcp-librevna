//! Direct SCPI smoke test against real hardware. No MCP involved.
//!
//! Its main job is to confirm the response formats this crate assumes, by
//! printing the raw bytes for the framings that matter and saying whether the
//! parsers agree. It was written before the hardware arrived, when those
//! framings were guesses from the upstream programming guide; it is kept as a
//! regression check, since a firmware change would break the same things.
//!
//! Usage, with LibreVNA-GUI running and a unit attached:
//!
//! ```text
//! cargo run --example smoke -- [host:port]
//! ```

use std::time::Duration;

use mcp_librevna::device::{DeviceLimits, StatusFlags};
use mcp_librevna::scpi::client::DEFAULT_SCPI_PORT;
use mcp_librevna::scpi::{ScpiClient, parse};

const TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| format!("127.0.0.1:{DEFAULT_SCPI_PORT}"));

    println!("Connecting to LibreVNA-GUI at {addr}");
    let mut client = ScpiClient::connect(&addr, TIMEOUT).await?;

    let idn = client.query("*IDN?").await?;
    println!("  *IDN? -> {idn:?}");
    match parse::parse_identity(&idn) {
        Ok(id) => println!("  parsed: serial {}, version {}", id.serial, id.version),
        Err(e) => println!("  !! identity did not parse: {e}"),
    }

    println!("\nConnecting to the device");
    client.command("DEV:CONN").await?;
    let connected = client.query("DEV:CONN?").await?;
    println!("  DEV:CONN? -> {connected:?}");
    if connected.trim().eq_ignore_ascii_case("Not connected") {
        anyhow::bail!("no LibreVNA attached; plug one in and try again");
    }

    println!("\nDevice limits");
    match DeviceLimits::query(&mut client).await {
        Ok(limits) => {
            println!(
                "  frequency : {} .. {} Hz",
                limits.min_freq_hz, limits.max_freq_hz
            );
            println!(
                "  power     : {} .. {} dBm",
                limits.min_power_dbm, limits.max_power_dbm
            );
            println!(
                "  IF bw     : {} .. {} Hz",
                limits.min_ifbw_hz, limits.max_ifbw_hz
            );
            println!(
                "  RBW       : {} .. {} Hz",
                limits.min_rbw_hz, limits.max_rbw_hz
            );
            println!("  max points: {}", limits.max_points);
            println!("\n  >> These should match the limits in src/mock.rs.");
        }
        Err(e) => println!("  !! limits did not parse: {e}"),
    }

    println!("\nStatus flags");
    match StatusFlags::query(&mut client).await {
        Ok(flags) => println!("  {flags:?}"),
        Err(e) => println!("  !! status did not parse: {e}"),
    }

    println!("\nConfiguring a short sweep");
    for command in [
        "DEV:MODE VNA",
        "VNA:FREQ:START 1000000000",
        "VNA:FREQ:STOP 2000000000",
        "VNA:ACQ:POINTS 51",
        "VNA:ACQ:IFBW 10000",
        "VNA:STIM:LVL -20",
        "VNA:ACQ:SINGLE TRUE",
    ] {
        client.command(command).await?;
        println!("  {command}");
    }

    println!("\nSweeping");
    client.command("VNA:ACQ:RUN").await?;
    for _ in 0..100 {
        let raw = client.query("VNA:ACQ:FINISHED?").await?;
        if parse::parse_bool("VNA:ACQ:FINISHED?", &raw)? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("  done");

    let traces = client.query("VNA:TRAC:LIST?").await?;
    println!("\n  VNA:TRAC:LIST? -> {traces:?}");
    let names = parse::parse_list(&traces);

    // Everything else is scalar and unambiguous; these are the framings worth
    // re-checking against a new firmware.
    println!("\n=== FORMAT CHECK ===");

    if let Some(first) = names.first() {
        let command = format!("VNA:TRAC:DATA? {first}");
        let raw = client
            .query_with_timeout(&command, Duration::from_secs(30))
            .await?;
        println!("\n{command}");
        println!(
            "  first 300 bytes: {:?}",
            raw.chars().take(300).collect::<String>()
        );
        println!("  total length   : {} bytes", raw.len());
        match parse::parse_complex_trace(&command, &raw) {
            Ok(points) => {
                println!("  PARSED OK: {} points", points.len());
                if let Some(p) = points.first() {
                    println!(
                        "  first point: {:.0} Hz, {:.6} {:+.6}j  ({:.2} dB)",
                        p.x,
                        p.re,
                        p.im,
                        p.magnitude_db()
                    );
                }
                if points.len() != 51 {
                    println!(
                        "  !! expected 51 points, got {} -- check the framing",
                        points.len()
                    );
                }
            }
            Err(e) => println!("  !! PARSE FAILED: {e}\n     Fix src/scpi/parse.rs to match."),
        }

        let at = format!("VNA:TRAC:AT? {first} 1500000000");
        let raw = client.query(&at).await?;
        println!("\n{at}");
        println!("  raw: {raw:?}");
        match parse::parse_complex_pair(&at, &raw) {
            Ok((re, im)) => println!("  PARSED OK: {re} {im:+}j"),
            Err(e) => println!("  !! PARSE FAILED: {e}"),
        }
    }

    // Touchstone spans many lines and ends with a blank one. It is also only
    // answered for a complete N-port matrix -- an incomplete set gets silence.
    let command = format!("VNA:TRAC:TOUCHSTONE? {}", names.join(" "));
    let raw = client
        .query_multiline(&command, Duration::from_secs(60))
        .await?;
    println!("\n{command}");
    println!(
        "  first 120 bytes: {:?}",
        raw.chars().take(120).collect::<String>()
    );
    println!("  total length   : {} bytes", raw.len());
    let rows = raw
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with(['#', '!']))
        .count();
    println!("  data rows      : {rows} (expected 51, one per sweep point)");
    if raw.lines().next().is_some_and(|l| l.starts_with('#')) {
        println!("  OK: multi-line Touchstone read whole.");
    } else {
        println!("  !! no option line -- check the blank-line framing.");
    }

    println!("\n=== Is the connection still in step afterwards? ===");
    let idn_again = client.query("*IDN?").await?;
    if idn_again == idn {
        println!("  OK: {idn_again:?}");
    } else {
        println!("  !! MISALIGNED: got {idn_again:?}, expected {idn:?}");
    }

    println!("\n=== Does a non-query command acknowledge anything? ===");
    println!("  If ScpiClient::command's *OPC? handshake works, sets are already");
    println!("  ordered correctly and nothing needs changing.");
    client.command("VNA:ACQ:POINTS 101").await?;
    let readback = client.query("VNA:ACQ:POINTS?").await?;
    println!("  set 101, read back {readback:?}");
    if readback.trim() == "101" {
        println!("  OK: sets and queries stay in step.");
    } else {
        println!("  !! MISMATCH -- responses may be misaligned by one exchange.");
    }

    println!("\nDone. Leaving the device connected and the sweep stopped.");
    client.command("VNA:ACQ:STOP").await?;
    Ok(())
}
