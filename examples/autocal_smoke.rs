//! Runs a full automatic SOLT calibration against real hardware, then checks
//! that the calibration actually corrects.
//!
//! Succeeding at every SCPI step proves very little on its own: a calibration
//! built against ideal standard definitions completes just as cleanly as one
//! built against the module's real coefficients, and reads as valid either way.
//! So the tool re-measures each standard through the finished calibration and
//! compares it against its own definition, and this reports that comparison.
//!
//! Needs the destructive tier: it writes coefficient files and discards any
//! existing calibration.

use std::path::Path;

use mcp_librevna::safety::Policy;
use mcp_librevna::server::{AutoCalArgs, ConnectArgs, LibreVnaServer, ServerConfig};
use rmcp::handler::server::wrapper::Parameters;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workdir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let server = LibreVnaServer::new(
        ServerConfig::default(),
        Policy::new(&workdir)?.with_destructive(true),
    );

    server
        .librevna_connect(Parameters(ConnectArgs { serial: None }))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("Running vna_cal_auto (several sweeps, please wait)");
    let r = server
        .vna_cal_auto(Parameters(AutoCalArgs {
            coefficient_set: None,
            timeout_s: Some(60),
        }))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .0;

    println!("  ok            : {}", r.ok);
    println!("  module        : {} set {}", r.serial, r.coefficient_set);
    println!("  type          : {}", r.cal_type);
    println!("  mapping       : {:?}", r.mapping);
    println!(
        "  sweep         : {:.0} .. {:.0} Hz, {} points",
        r.sweep.start_hz, r.sweep.stop_hz, r.sweep.points
    );
    println!("  calibration   : {:?}", r.calibration);

    println!("\n  kit standards installed:");
    for s in &r.standards {
        println!(
            "    [{}] {:8} {:16} {} points  <- {}",
            s.index,
            s.kind,
            s.coefficient,
            s.points,
            Path::new(&s.file).file_name().unwrap().to_string_lossy()
        );
    }
    println!("\n  measurements taken:");
    for st in &r.steps {
        println!(
            "    entry {} ports {:?}  {}",
            st.entry, st.ports, st.standard
        );
    }
    for w in &r.warnings {
        println!("  !! {w}");
    }

    println!("\n=== residuals: each standard re-measured through the calibration ===");
    println!("  A corrected standard should read back as its own coefficients describe it,");
    println!("  not as zero. Tracking the real definition is the evidence that the module's");
    println!("  data is in use: with ideal standards the load would correct towards a");
    println!("  perfect match instead.\n");
    println!(
        "  {:44} {:>5} {:>10} {:>10} {:>9}",
        "standard", "param", "expected", "measured", "deviation"
    );
    for r in &r.residuals {
        println!(
            "  {:44} {:>5} {:>9.2}dB {:>9.2}dB {:>8.2}dB  {}",
            r.standard,
            r.parameter,
            r.expected_db,
            r.measured_db,
            r.deviation_db,
            if r.ok { "ok" } else { "FAIL" }
        );
    }

    server
        .librevna_disconnect()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if r.ok {
        println!("\nCalibration is active and reproduces every standard it was built from.");
        Ok(())
    } else {
        anyhow::bail!("the calibration completed but does not reproduce its own standards")
    }
}
