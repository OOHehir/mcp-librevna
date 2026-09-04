//! Integration tests for the MCP tool surface, driven against the mock.
//!
//! These exercise the paths that matter for safety: that a refused request is
//! refused *before* anything reaches the instrument, that gated tools stay shut
//! without their flag, and that every measurement carries the caveats an agent
//! needs in order not to over-trust it.

use std::time::Duration;

use mcp_librevna::mock::{Faults, MockServer};
use mcp_librevna::safety::Policy;
use mcp_librevna::server::*;
use rmcp::handler::server::wrapper::Parameters;

struct Harness {
    mock: MockServer,
    server: LibreVnaServer,
}

async fn harness_with(configure: impl FnOnce(Policy) -> Policy) -> Harness {
    let mock = MockServer::spawn().await.expect("mock should start");
    let workdir = std::env::temp_dir().join(format!(
        "mcp-librevna-tools-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&workdir).unwrap();

    let policy = configure(Policy::new(&workdir).unwrap());
    let config = ServerConfig {
        addr: mock.addr(),
        timeout: Duration::from_secs(5),
        sweep_timeout: Duration::from_secs(10),
    };

    let server = LibreVnaServer::new(config, policy);
    server
        .librevna_connect(Parameters(ConnectArgs { serial: None }))
        .await
        .expect("connect should succeed against the mock");

    Harness { mock, server }
}

async fn harness() -> Harness {
    harness_with(|p| p).await
}

fn sweep_args() -> ConfigureSweepArgs {
    ConfigureSweepArgs {
        start_hz: None,
        stop_hz: None,
        center_hz: None,
        span_hz: None,
        points: None,
        ifbw_hz: None,
        power_dbm: None,
        averaging: None,
        logarithmic: None,
    }
}

#[tokio::test]
async fn connect_reports_the_devices_own_limits() {
    let mock = MockServer::spawn().await.unwrap();
    let workdir = std::env::temp_dir();
    let server = LibreVnaServer::new(
        ServerConfig {
            addr: mock.addr(),
            timeout: Duration::from_secs(5),
            sweep_timeout: Duration::from_secs(10),
        },
        Policy::new(&workdir).unwrap(),
    );

    let result = server
        .librevna_connect(Parameters(ConnectArgs { serial: None }))
        .await
        .unwrap()
        .0;

    assert_eq!(result.identity.serial, "MOCK0001");
    assert_eq!(result.limits.max_freq_hz, 6.0e9);
    assert_eq!(result.limits.max_points, 4501);
    // Only the two default tiers, with nothing opted into.
    assert_eq!(result.enabled_tiers, vec!["read-only", "measure"]);
}

#[tokio::test]
async fn an_out_of_range_request_sends_no_scpi_at_all() {
    let h = harness().await;
    let before = h.mock.command_log().len();

    let result = h
        .server
        .vna_configure_sweep(Parameters(ConfigureSweepArgs {
            start_hz: Some(8.0e9),
            stop_hz: Some(9.0e9),
            ..sweep_args()
        }))
        .await;

    assert!(result.is_err(), "8-9 GHz is beyond a 6 GHz instrument");
    assert_eq!(
        h.mock.command_log().len(),
        before,
        "a rejected request must not touch the instrument"
    );
}

#[tokio::test]
async fn an_excessive_point_count_is_refused_before_transmission() {
    let h = harness().await;
    let before = h.mock.command_log().len();

    let result = h
        .server
        .vna_configure_sweep(Parameters(ConfigureSweepArgs {
            points: Some(9999),
            ..sweep_args()
        }))
        .await;

    assert!(result.is_err());
    assert_eq!(h.mock.command_log().len(), before);
}

#[tokio::test]
async fn stimulus_above_the_ceiling_is_refused_and_nothing_is_sent() {
    let h = harness().await;
    let before = h.mock.command_log().len();

    // The device permits 0 dBm; the default ceiling is -10 dBm.
    let result = h
        .server
        .vna_configure_sweep(Parameters(ConfigureSweepArgs {
            power_dbm: Some(0.0),
            ..sweep_args()
        }))
        .await;

    assert!(result.is_err(), "0 dBm exceeds the default ceiling");
    assert_eq!(h.mock.command_log().len(), before);
    assert_eq!(h.mock.setting("VNA:STIM:LVL"), "-10");
}

#[tokio::test]
async fn a_valid_sweep_configuration_is_applied() {
    let h = harness().await;
    h.server
        .vna_configure_sweep(Parameters(ConfigureSweepArgs {
            center_hz: Some(2.44e9),
            span_hz: Some(2.0e8),
            points: Some(1001),
            ifbw_hz: Some(1000.0),
            power_dbm: Some(-20.0),
            ..sweep_args()
        }))
        .await
        .unwrap();

    assert_eq!(h.mock.setting("VNA:ACQ:POINTS"), "1001");
    assert_eq!(h.mock.setting("VNA:STIM:LVL"), "-20");
    let start: f64 = h.mock.setting("VNA:FREQ:START").parse().unwrap();
    let stop: f64 = h.mock.setting("VNA:FREQ:STOP").parse().unwrap();
    assert_eq!((start, stop), (2.34e9, 2.54e9));
}

#[tokio::test]
async fn emission_is_refused_without_the_flag_and_the_port_stays_off() {
    let h = harness().await;
    let result = h
        .server
        .gen_configure(Parameters(GenArgs {
            freq_hz: 1.0e9,
            power_dbm: -20.0,
            port: 1,
        }))
        .await;

    let err = result.err().expect("emission should be gated");
    assert!(
        err.message.contains("--allow-emission"),
        "the refusal should name the flag: {}",
        err.message
    );
    assert_eq!(h.mock.setting("GEN:PORT"), "0", "port must remain disabled");
}

#[tokio::test]
async fn emission_works_once_the_flag_is_given() {
    let h = harness_with(|p| p.with_emission(true)).await;
    h.server
        .gen_configure(Parameters(GenArgs {
            freq_hz: 1.0e9,
            power_dbm: -20.0,
            port: 2,
        }))
        .await
        .unwrap();

    assert_eq!(h.mock.setting("GEN:PORT"), "2");
    assert_eq!(h.mock.setting("GEN:FREQUENCY"), "1000000000");
}

#[tokio::test]
async fn the_generator_is_configured_before_the_port_is_enabled() {
    // Enabling the port first would briefly emit at whatever the previous
    // frequency and level happened to be.
    let h = harness_with(|p| p.with_emission(true)).await;
    h.server
        .gen_configure(Parameters(GenArgs {
            freq_hz: 2.0e9,
            power_dbm: -25.0,
            port: 1,
        }))
        .await
        .unwrap();

    let log = h.mock.command_log();
    let port_at = log.iter().position(|c| c.starts_with("GEN:PORT")).unwrap();
    let freq_at = log
        .iter()
        .position(|c| c.starts_with("GEN:FREQUENCY"))
        .unwrap();
    let level_at = log.iter().position(|c| c.starts_with("GEN:LVL")).unwrap();
    assert!(freq_at < port_at, "frequency must be set before the port");
    assert!(level_at < port_at, "level must be set before the port");
}

#[tokio::test]
async fn the_generator_can_always_be_switched_off() {
    // The stop button must not depend on the tier that started the emission.
    let h = harness().await;
    h.server
        .gen_off()
        .await
        .expect("gen_off must never be gated");
    h.server
        .librevna_rf_off()
        .await
        .expect("rf_off must never be gated");
    assert_eq!(h.mock.setting("GEN:PORT"), "0");
}

#[tokio::test]
async fn destructive_operations_are_gated() {
    let h = harness().await;
    assert!(h.server.vna_cal_reset().await.is_err());
    assert!(
        h.server
            .vna_export_touchstone(Parameters(ExportArgs {
                traces: vec!["S11".into()],
                filename: "out.s2p".into(),
            }))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn raw_scpi_is_gated_and_firmware_update_is_refused_even_when_enabled() {
    let open = harness_with(|p| p.with_manual_hardware(true)).await;

    // With the tier enabled, an ordinary command goes through.
    open.server
        .scpi_raw(Parameters(RawArgs {
            command: "*IDN?".into(),
        }))
        .await
        .expect("raw SCPI should work once enabled");

    // Firmware flashing stays refused regardless of tier.
    let err = open
        .server
        .scpi_raw(Parameters(RawArgs {
            command: "DEV:UPDATE /tmp/firmware.bin".into(),
        }))
        .await
        .err()
        .expect("firmware update must always be refused");
    assert!(err.message.contains("not available"), "{}", err.message);
}

#[tokio::test]
async fn a_path_escaping_the_workdir_is_refused() {
    let h = harness_with(|p| p.with_destructive(true)).await;
    let err = h
        .server
        .vna_export_touchstone(Parameters(ExportArgs {
            traces: vec!["S11".into()],
            filename: "../../etc/escaped.s2p".into(),
        }))
        .await
        .err()
        .expect("traversal should be refused");
    assert!(err.message.contains("outside"), "{}", err.message);
}

#[tokio::test]
async fn measurements_warn_when_no_calibration_is_active() {
    let h = harness().await;
    let result = h
        .server
        .vna_sweep(Parameters(SweepArgs { timeout_s: Some(5) }))
        .await
        .unwrap()
        .0;

    assert!(
        result.warnings.iter().any(|w| w.contains("No calibration")),
        "uncalibrated data must say so: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn device_error_flags_reach_the_result() {
    let h = harness().await;
    h.mock.set_faults(Faults {
        adc_overload: true,
        ..Default::default()
    });

    let result = h
        .server
        .vna_sweep(Parameters(SweepArgs { timeout_s: Some(5) }))
        .await
        .unwrap()
        .0;

    assert!(result.flags.adc_overload);
    assert!(
        result.warnings.iter().any(|w| w.contains("ADC overload")),
        "an overloaded ADC must be surfaced: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn a_stalled_sweep_times_out_rather_than_hanging() {
    let h = harness().await;
    h.mock.set_faults(Faults {
        stall_sweep: true,
        ..Default::default()
    });

    let result = h
        .server
        .vna_sweep(Parameters(SweepArgs { timeout_s: Some(1) }))
        .await;
    assert!(result.is_err(), "a sweep that never finishes must time out");
}

#[tokio::test]
async fn reading_a_trace_returns_a_summary_not_every_point() {
    let h = harness().await;
    h.server
        .vna_configure_sweep(Parameters(ConfigureSweepArgs {
            center_hz: Some(2.44e9),
            span_hz: Some(2.0e8),
            points: Some(1001),
            ..sweep_args()
        }))
        .await
        .unwrap();

    let result = h
        .server
        .vna_read(Parameters(ReadArgs {
            trace: Some("S11".into()),
            envelope_points: Some(32),
        }))
        .await
        .unwrap()
        .0;

    assert_eq!(result.points, 1001, "the full sweep was measured");
    assert!(
        result.envelope.len() <= 32,
        "but only the envelope is returned: {} bins",
        result.envelope.len()
    );
    assert!(result.minimum.is_some());
}

#[tokio::test]
async fn a_marker_outside_the_swept_range_is_refused_not_extrapolated() {
    let h = harness().await;
    h.server
        .vna_configure_sweep(Parameters(ConfigureSweepArgs {
            start_hz: Some(2.4e9),
            stop_hz: Some(2.5e9),
            ..sweep_args()
        }))
        .await
        .unwrap();

    let err = h
        .server
        .vna_marker(Parameters(MarkerArgs {
            trace: "S11".into(),
            freq_hz: 1.0e9,
        }))
        .await
        .err()
        .expect("a frequency outside the sweep should be refused");
    assert!(err.message.contains("outside"), "{}", err.message);
}

#[tokio::test]
async fn every_reported_number_survives_json() {
    // Infinities and NaN serialise to `null`, which would reach the agent as a
    // missing field rather than a value.
    let h = harness().await;
    h.server
        .vna_configure_sweep(Parameters(ConfigureSweepArgs {
            center_hz: Some(2.44e9),
            span_hz: Some(2.0e8),
            points: Some(501),
            ..sweep_args()
        }))
        .await
        .unwrap();

    let analysis = h
        .server
        .vna_analyze(Parameters(AnalyzeArgs {
            trace: Some("S11".into()),
            depth_db: Some(3.0),
        }))
        .await
        .unwrap()
        .0;

    let json = serde_json::to_value(&analysis).unwrap();
    assert!(
        !json.to_string().contains("null"),
        "no field should serialise to null: {json}"
    );
}

#[tokio::test]
async fn an_under_resolved_feature_is_flagged_rather_than_reported_plainly() {
    // A 200 MHz span cannot characterise a ~60 kHz notch; the Q would be an
    // artefact of the point spacing.
    let h = harness().await;
    h.server
        .vna_configure_sweep(Parameters(ConfigureSweepArgs {
            center_hz: Some(2.44e9),
            span_hz: Some(2.0e8),
            points: Some(501),
            ..sweep_args()
        }))
        .await
        .unwrap();

    let result = h
        .server
        .vna_analyze(Parameters(AnalyzeArgs {
            trace: Some("S11".into()),
            depth_db: Some(3.0),
        }))
        .await
        .unwrap()
        .0;

    assert!(result.summary.under_resolved);
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("Narrow the span")),
        "the agent must be told the Q is unreliable: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn status_is_answerable_before_connecting() {
    let mock = MockServer::spawn().await.unwrap();
    let server = LibreVnaServer::new(
        ServerConfig {
            addr: mock.addr(),
            timeout: Duration::from_secs(5),
            sweep_timeout: Duration::from_secs(10),
        },
        Policy::new(std::env::temp_dir()).unwrap(),
    );

    let status = server.librevna_status().await.unwrap().0;
    assert!(!status.connected);
    assert!(status.warning.unwrap().contains("librevna_connect"));
}

#[tokio::test]
async fn a_calibration_type_the_device_does_not_offer_is_refused() {
    // The instrument ignores an unknown type in silence and a set draws no
    // reply, so *OPC? still returns 1. Without this check the activation looks
    // like it worked and every later reading is reported as corrected.
    let harness = harness().await;

    let error = harness
        .server
        .vna_cal_activate(Parameters(CalActivateArgs {
            cal_type: "SOLT".into(),
        }))
        .await
        .err()
        .expect("a type the device does not offer should be refused");

    let message = format!("{error:?}");
    assert!(
        message.contains("SOLT_12"),
        "should list the real names: {message}"
    );
    assert!(
        !harness
            .mock
            .command_log()
            .iter()
            .any(|c| c == "VNA:CAL:ACTIVATE SOLT"),
        "nothing should have been sent: {:?}",
        harness.mock.command_log()
    );
}

#[tokio::test]
async fn a_calibration_type_the_device_offers_is_activated() {
    let harness = harness().await;

    let result = harness
        .server
        .vna_cal_activate(Parameters(CalActivateArgs {
            cal_type: "SOLT_12".into(),
        }))
        .await
        .expect("a type the device offers should activate");

    assert_eq!(result.0.active.as_deref(), Some("SOLT_12"));
}
