//! End-to-end tests of the SCPI client against the mock server.

use std::time::Duration;

use mcp_librevna::mock::{Faults, MockServer};
use mcp_librevna::scpi::{ScpiClient, parse};

async fn connected() -> (MockServer, ScpiClient) {
    let mock = MockServer::spawn().await.expect("mock should start");
    let client = ScpiClient::connect(&mock.addr(), Duration::from_secs(5))
        .await
        .expect("client should connect to the mock");
    (mock, client)
}

#[tokio::test]
async fn identifies_the_instrument() {
    let (_mock, mut client) = connected().await;
    let raw = client.query("*IDN?").await.unwrap();
    let id = parse::parse_identity(&raw).unwrap();
    assert_eq!(id.manufacturer, "LibreVNA");
    assert_eq!(id.serial, "MOCK0001");
}

#[tokio::test]
async fn settings_round_trip_through_their_queries() {
    let (_mock, mut client) = connected().await;
    client.command("VNA:ACQ:POINTS 1001").await.unwrap();
    let points = client.query("VNA:ACQ:POINTS?").await.unwrap();
    assert_eq!(
        parse::parse_usize("VNA:ACQ:POINTS?", &points).unwrap(),
        1001
    );
}

#[tokio::test]
async fn commands_stay_in_order_across_many_exchanges() {
    // The *OPC? handshake exists to keep sets ordered; if responses were being
    // misattributed across exchanges this interleaving would drift.
    let (mock, mut client) = connected().await;
    for points in [101usize, 201, 401, 801] {
        client
            .command(&format!("VNA:ACQ:POINTS {points}"))
            .await
            .unwrap();
        let read = client.query("VNA:ACQ:POINTS?").await.unwrap();
        assert_eq!(parse::parse_usize("q", &read).unwrap(), points);
    }
    assert_eq!(mock.setting("VNA:ACQ:POINTS"), "801");
}

#[tokio::test]
async fn derived_frequency_settings_stay_consistent() {
    let (_mock, mut client) = connected().await;
    client.command("VNA:FREQ:CENTER 2440000000").await.unwrap();
    client.command("VNA:FREQ:SPAN 100000000").await.unwrap();

    let start = parse::parse_f64("s", &client.query("VNA:FREQ:START?").await.unwrap()).unwrap();
    let stop = parse::parse_f64("s", &client.query("VNA:FREQ:STOP?").await.unwrap()).unwrap();
    assert_eq!(start, 2.39e9);
    assert_eq!(stop, 2.49e9);
}

#[tokio::test]
async fn trace_data_parses_into_points() {
    let (_mock, mut client) = connected().await;
    client.command("VNA:FREQ:START 2400000000").await.unwrap();
    client.command("VNA:FREQ:STOP 2480000000").await.unwrap();
    client.command("VNA:ACQ:POINTS 201").await.unwrap();

    let raw = client.query("VNA:TRAC:DATA? S11").await.unwrap();
    let points = parse::parse_complex_trace("VNA:TRAC:DATA?", &raw).unwrap();

    assert_eq!(points.len(), 201);
    assert_eq!(points[0].x, 2.4e9);
    assert_eq!(points[200].x, 2.48e9);

    // The synthesised resonator should dip near 2.44 GHz.
    let deepest = points
        .iter()
        .min_by(|a, b| a.magnitude_db().total_cmp(&b.magnitude_db()))
        .unwrap();
    assert!(
        (deepest.x - 2.44e9).abs() < 1e7,
        "resonance should sit near 2.44 GHz, found {} Hz",
        deepest.x
    );
}

#[tokio::test]
async fn spectrum_data_parses_into_pairs() {
    let (_mock, mut client) = connected().await;
    let raw = client.query("SA:TRAC:DATA? PORT1").await.unwrap();
    let points = parse::parse_scalar_trace("SA:TRAC:DATA?", &raw).unwrap();
    assert!(points.len() > 100);
    let peak = points.iter().max_by(|a, b| a.y.total_cmp(&b.y)).unwrap();
    assert!(peak.y > -40.0, "carrier should stand well above the floor");
}

#[tokio::test]
async fn injected_error_flags_are_visible() {
    let (mock, mut client) = connected().await;
    assert_eq!(client.query("DEV:STA:ADCOVERLOAD?").await.unwrap(), "FALSE");

    mock.set_faults(Faults {
        adc_overload: true,
        ..Default::default()
    });
    let raw = client.query("DEV:STA:ADCOVERLOAD?").await.unwrap();
    assert!(parse::parse_bool("DEV:STA:ADCOVERLOAD?", &raw).unwrap());
}

#[tokio::test]
async fn a_query_that_never_answers_times_out_rather_than_hanging() {
    let mock = MockServer::spawn().await.unwrap();
    let mut client = ScpiClient::connect(&mock.addr(), Duration::from_millis(200))
        .await
        .unwrap();

    // Nothing listens on a bare TCP socket for a response the mock never sends,
    // so ask a question the mock deliberately answers with silence.
    mock.set_faults(Faults {
        drop_connection: true,
        ..Default::default()
    });

    let result = client.query("VNA:ACQ:FINISHED?").await;
    assert!(
        result.is_err(),
        "a dropped connection must surface as an error, not a hang"
    );
}

#[tokio::test]
async fn connecting_to_a_dead_port_reports_gui_unavailable() {
    // Port 1 on loopback is reliably closed for an unprivileged process.
    let result = ScpiClient::connect("127.0.0.1:1", Duration::from_millis(500)).await;
    let err = result.err().expect("connection should fail");
    assert!(
        matches!(err, mcp_librevna::VnaError::GuiUnavailable { .. }),
        "expected GuiUnavailable, got {err:?}"
    );
}

#[tokio::test]
async fn a_multi_line_touchstone_response_is_read_whole() {
    // The response is a header, one row per sweep point and a blank line to
    // close; a single-line read would return only the header.
    let (_mock, mut client) = connected().await;
    client.command("VNA:ACQ:POINTS 11").await.unwrap();

    let text = client
        .query_multiline("VNA:TRAC:TOUCHSTONE? S11", Duration::from_secs(5))
        .await
        .unwrap();

    let rows: Vec<&str> = text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with(['#', '!']))
        .collect();
    assert_eq!(rows.len(), 11, "every sweep point should arrive:\n{text}");
    assert!(
        text.lines().any(|l| l.starts_with('#')),
        "the Touchstone option line should survive: {text}"
    );
    assert!(
        !text.contains("\\n"),
        "rows are real newlines, not escapes: {text}"
    );
}

#[tokio::test]
async fn the_connection_stays_in_step_after_a_multi_line_response() {
    // Unread rows left queued make every later query return the previous
    // answer: plausible numbers keep arriving and nothing looks wrong.
    let (_mock, mut client) = connected().await;
    client.command("VNA:ACQ:POINTS 51").await.unwrap();
    client
        .query_multiline("VNA:TRAC:TOUCHSTONE? S11", Duration::from_secs(5))
        .await
        .unwrap();

    let raw = client.query("*IDN?").await.unwrap();
    assert!(
        raw.starts_with("LibreVNA,"),
        "the next query should get its own answer, got {raw:?}"
    );
}

#[tokio::test]
async fn a_query_the_instrument_does_not_recognise_says_so_rather_than_timing_out() {
    // Unrecognised queries are answered with silence, so the error has to
    // explain that rather than blaming a slow instrument.
    let (_mock, mut client) = connected().await;
    let error = client
        .query_multiline("VNA:TRAC:NONSENSE?", Duration::from_millis(300))
        .await
        .expect_err("an unknown query should not appear to succeed")
        .to_string();
    assert!(
        error.contains("silence"),
        "the error should name the real cause: {error}"
    );
}

#[tokio::test]
async fn an_unterminated_multi_line_reply_is_drained_not_left_queued() {
    // `*LST?` answers over many lines with no terminator. Reading one line and
    // leaving the rest queued is what desynchronises the session.
    let (_mock, mut client) = connected().await;

    let text = client
        .query_raw("*LST?", Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        text.lines().count() > 5,
        "the whole list should arrive, got {text:?}"
    );

    let idn = client.query("*IDN?").await.unwrap();
    assert!(
        idn.starts_with("LibreVNA,"),
        "the next query should get its own answer, got {idn:?}"
    );
}

#[tokio::test]
async fn query_raw_still_ends_on_the_blank_line_of_a_terminated_reply() {
    let (_mock, mut client) = connected().await;
    client.command("VNA:ACQ:POINTS 7").await.unwrap();

    let text = client
        .query_raw("VNA:TRAC:TOUCHSTONE? S11", Duration::from_secs(5))
        .await
        .unwrap();
    let rows = text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with(['#', '!']))
        .count();
    assert_eq!(rows, 7, "every row should arrive:\n{text}");
    assert!(
        client
            .query("*IDN?")
            .await
            .unwrap()
            .starts_with("LibreVNA,")
    );
}

#[tokio::test]
async fn a_desynchronised_session_refuses_further_commands() {
    // Returning the previous answer to each new query is worse than an error:
    // the numbers stay plausible while belonging to the wrong question.
    let (_mock, mut client) = connected().await;
    client.command("VNA:ACQ:POINTS 4501").await.unwrap();

    // Too short to read the whole Touchstone body, so rows are left queued.
    let partial = client
        .query_multiline("VNA:TRAC:TOUCHSTONE? S11", Duration::from_millis(1))
        .await;
    assert!(partial.is_err(), "the truncated read should fail");

    let after = client.query("*IDN?").await;
    let error = after
        .expect_err("a desynchronised session must not answer")
        .to_string();
    assert!(error.contains("out of step"), "{error}");
}
