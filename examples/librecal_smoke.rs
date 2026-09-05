//! Exercises LibreCAL support against real hardware.
//!
//! Kept as a regression check alongside `smoke.rs`: run it after a firmware
//! change on either device. It needs a LibreVNA-GUI listening on 19542 with a
//! device connected, and a LibreCAL on USB with its ports cabled to the VNA.
//!
//! Nothing here writes to the module. Every port is returned to NONE.

use std::time::Duration;

use mcp_librevna::librecal;
use mcp_librevna::safety::Policy;
use mcp_librevna::server::{ConnectArgs, LibreCalVerifyArgs, LibreVnaServer, ServerConfig};
use rmcp::handler::server::wrapper::Parameters;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Discovering LibreCAL modules on USB");
    let found = librecal::discover();
    if found.is_empty() {
        println!("  none found -- nothing to check");
        return Ok(());
    }
    for m in &found {
        println!("  {}  serial {}", m.path.display(), m.usb_serial);
    }

    let server = LibreVnaServer::new(ServerConfig::default(), Policy::new(".")?);

    println!("\nlibrecal_status");
    let status = server
        .librecal_status()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .0;
    println!("  present   : {}", status.present);
    println!("  serial    : {:?}", status.serial);
    println!("  firmware  : {:?}", status.firmware);
    println!("  ports     : {:?}", status.ports);
    println!(
        "  temp      : {:?} C  stable {:?}",
        status.temperature_c, status.temperature_stable
    );
    println!("  heater    : {:?} W", status.heater_power_w);
    println!("  coeff sets: {:?}", status.coefficient_sets);
    println!("  standards : {:?}", status.port_standards);
    if let Some(w) = &status.warning {
        println!("  !! {w}");
    }

    println!("\nConnecting to the VNA");
    server
        .librevna_connect(Parameters(ConnectArgs { serial: None }))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("\nlibrecal_verify (this sweeps several times, please wait)");
    let verify = server
        .librecal_verify(Parameters(LibreCalVerifyArgs {
            timeout_s: Some(30),
        }))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .0;

    println!("  overall ok : {}", verify.ok);
    println!("  serial     : {}", verify.serial);
    println!(
        "  temperature: {:.2} C  stable {}",
        verify.temperature_c, verify.temperature_stable
    );
    println!("  mapping    : {:?}", verify.mapping);
    println!();
    for p in &verify.ports {
        print!("  cal port {} -> ", p.cal_port);
        match p.vna_port {
            Some(v) => print!("VNA {v}"),
            None => print!("(unused)"),
        }
        println!("   [{}] ok={}", p.verdict, p.ok);
        if let (Some(l), Some(o)) = (p.load_db, p.open_db) {
            println!(
                "      LOAD {l:7.2} dB   OPEN {o:7.2} dB   SHORT {:7.2} dB   change {:6.2} dB",
                p.short_db.unwrap_or(f64::NAN),
                o - l
            );
        }
        println!("      {}", p.detail);
    }
    for w in &verify.warnings {
        println!("  !! {w}");
    }

    println!("\nlibrecal_status again, to confirm every port was released");
    let after = server
        .librecal_status()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .0;
    println!("  standards : {:?}", after.port_standards);
    if after.port_standards.iter().any(|s| s != "NONE") {
        println!("  !! a port was left terminated");
    }

    server
        .librevna_disconnect()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let _ = Duration::from_secs(0);
    println!("\nDone.");
    Ok(())
}
